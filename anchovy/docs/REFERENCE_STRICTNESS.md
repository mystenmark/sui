# Reference deserialization strictness (sui-types vs. format__sui.yaml.snap)

Sources read (all read-only):

- sui repo `/Users/marklogan/repos/sui` at commit `14c59d81505b` (workspace version 1.74.0). Paths below
  are relative to it unless absolute. `ST` = `crates/sui-types/src`.
- `bcs` 0.1.6 (Cargo.lock:2683): `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/bcs-0.1.6/src/{de.rs,lib.rs}`
- fastcrypto: sui pins rev `239eed70b763f89fdb1e6c6866fe389690945c45` (Cargo.toml:620). NOTE: the only
  *checkout* on disk (`~/.cargo/git/checkouts/fastcrypto-9995504e1c5344d5/076bf5a`) is a DIFFERENT, newer
  rev (076bf5ad, 2026-08-28). I read the pinned rev out of the bare db with
  `git -C ~/.cargo/git/db/fastcrypto-9995504e1c5344d5 show 239eed70...:<path>`. Citations marked `FC:` are
  paths inside that rev.
- serde 1.0.228 (serde_core), serde_with 3.15.1, serde_json 1.0.145, roaring 0.11.4, nonempty 0.9.0,
  blst 0.3.16, ark-ff 0.4.2, base64ct 1.8.0, p256 0.13.2 / sec1 0.7.1 / primeorder 0.13.0, passkey-types 0.4.0.

Conventions: "depth +1" means the type calls a bcs entry point that consumes one unit of the
500-deep container budget while its contents are being read.

---------------------------------------------------------------------------------------------------

## 0. The most important deltas vs. the YAML (read this first)

1. **The YAML is incomplete for two enums.** `CompressedSignature` has a 5th variant
   `Passkey(PasskeyAuthenticatorAsBytes)` (index 4, newtype struct over `Vec<u8>`), and `PublicKey` has a
   5th variant `Passkey(Secp256r1PublicKeyAsBytes)` (index 4, 33 raw bytes). `ST/crypto.rs:1759-1771`,
   `ST/crypto.rs:264-270`. The tracer never reached them. Both are accepted by the reference.
2. **`GenericSignature` is not an opaque `SEQ U8`.** Deserialize parses the inner bytes: flag byte, fixed
   lengths, a nested `bcs::from_bytes` of MultiSig / MultiSigLegacy / ZkLoginAuthenticator /
   PasskeyAuthenticator, plus semantic validation. For zkLogin and passkey that includes base64url decoding,
   **serde_json parsing**, and for passkey **P-256 point decompression and ECDSA scalar range checks**.
   The MultiSig fallback to `MultiSigLegacy` additionally needs base64, roaring-bitmap parsing and
   ed25519 / secp256k1 / P-256 public-key validation. See section 6.
3. **`AuthorityQuorumSignInfo.signature` ([u8;48]) must be a valid compressed BLS12-381 G1 encoding**
   (on-curve check incl. a field sqrt; no subgroup check), and **`signers_map` is parsed as a Roaring bitmap
   at deser time** (strict format checks, cardinality <= 150, trailing garbage inside the BYTES is ignored).
   Section 7.
4. **`SenderSignedData`: exactly one element, and the intent must be exactly `00 00 00`.** Section 5 / 8.
5. **`Identifier` is validated on deser and is invisible in the YAML** (shows as `STR`): `StructTag.module`,
   `StructTag.name`, `ModuleId.name`, `Event.transaction_module`. But `StructInput.module/name`,
   `ProgrammableMoveCall.module/function`, `TypeOrigin.*`, `ExecutionTimeObservationKey::MoveEntryPoint.*`,
   `MoveLocation.function_name` are plain `String` (UTF-8 only). Identifier adds **no** container depth.
6. **`Owner::Party` / `RawPartySerde`**: the two `U64`s are `ObjectPermissions` with bit validation;
   members must be strictly sorted; the ConsensusAddressOwner-equivalent form is rejected. Section 9.
7. `AccumulatorValue::EventDigest` is a `NonEmpty` -> empty SEQ rejected. `Duration` rejects
   `secs + nanos/1e9` overflow. `BTreeMap`s require strictly increasing *serialized key bytes*.

---------------------------------------------------------------------------------------------------

## 1. `bcs` 0.1.6 rules

Version: `Cargo.lock:2683-2684` -> `bcs 0.1.6`.

### from_bytes / trailing bytes
`de.rs:37-45`: `Deserializer::new(bytes, MAX_CONTAINER_DEPTH)`, `T::deserialize`, then `deserializer.end()?`.
`de.rs:390-396`: `end()` returns `Err(Error::RemainingInput)` unless input is empty. So any trailing byte
rejects. This applies again to every *nested* `bcs::from_bytes` (MultiSig, ZkLogin, Passkey inner bytes) and
each nested call gets a **fresh** depth budget of 500.

### uleb128 (`de.rs:261-281`)
```rust
for shift in (0..32).step_by(7) {            // at most 5 bytes: shifts 0,7,14,21,28
    let byte = self.next()?; let digit = byte & 0x7f;
    value |= u64::from(digit) << shift;
    if digit == byte {                        // high bit clear => last byte
        if shift > 0 && digit == 0 { return Err(Error::NonCanonicalUleb128Encoding); }
        return u32::try_from(value).map_err(|_| Error::IntegerOverflowDuringUleb128Decoding);
    }
}
Err(Error::IntegerOverflowDuringUleb128Decoding)   // 5 bytes all with continuation bit
```
- Max value u32::MAX. 5th byte may only be 0x01..=0x0f (0x00 => non-canonical; >0x0f => overflow;
  >=0x80 => overflow because loop ends).
- Non-canonical = last byte is 0x00 and it is not the first byte. `0x00` alone is fine. `80 00` rejected.
- Used for sequence/map/bytes/string lengths AND enum variant indices (`de.rs:796-803`).

### MAX_SEQUENCE_LENGTH (`lib.rs:312`, `de.rs:283-289`)
`(1 << 31) - 1`. `parse_length` rejects `len > MAX_SEQUENCE_LENGTH` (`ExceededMaxLen`). Applies to seq, map,
bytes, str. NOT applied to enum variant indices (those just fail as unknown variant).
Bytes/str: `self.input.get(..len).ok_or(Error::Eof)` (`de.rs:404-409`) - no allocation before the check.

### MAX_CONTAINER_DEPTH = 500 (`lib.rs:315`, `de.rs:417-429`)
```rust
fn enter_named_container(&mut self, name) { if self.max_remaining_depth == 0 { return Err(ExceededContainerDepthLimit(name)); } self.max_remaining_depth -= 1; }
```
So 500 nested named containers are OK, the 501st fails. Calls that increment:
| serde call | depth? | cite |
|---|---|---|
| `deserialize_struct` | yes | de.rs:649-662 |
| `deserialize_newtype_struct` | yes | de.rs:601-609 |
| `deserialize_tuple_struct` | yes | de.rs:626-639 |
| `deserialize_unit_struct` | yes | de.rs:591-599 |
| `deserialize_enum` | yes (once per enum; struct/tuple/newtype *variants* add nothing: `struct_variant`/`tuple_variant` call `deserialize_tuple`, de.rs:823-835) | de.rs:664-677 |
| `deserialize_tuple` | no | de.rs:619-624 |
| `deserialize_seq` | no | de.rs:611-617 |
| `deserialize_map` | no | de.rs:641-647 |
| `deserialize_option` | no | de.rs:571-582 |
| bytes/str/ints/bool/unit | no | |

Consequences for the YAML: every YAML entry of kind STRUCT / NEWTYPESTRUCT / TUPLESTRUCT / ENUM costs 1
while its body is read (the YAML was traced through the same serde calls). Types that are NOT in the YAML
or behave specially:
- cost 0: `Identifier` (DisplayFromStr -> `deserialize_str`), `ObjectPermissions` (`try_from = "u64"`),
  `SizeOneVec`, `NonEmpty` (`try_from = "Vec<T>"`), `IntentScope/IntentVersion/AppId` (serde_repr -> u8),
  `BytesRepresentation<N>` and BLS signature (`SerializationHelper` is `#[serde(transparent)]`),
  `Signature` (Bytes), `SuiBitmap`, `Box<T>`, `Option<T>`, `Arc`.
- cost 1: `Party` (via `RawPartySerde` struct), `Object` (via `ObjectInner` renamed "Object"),
  `Envelope` (remote derive, name adapter only changes the name), `EmptySignInfo` (struct, zero fields,
  zero bytes), `Duration` (serde: `deserialize_struct("Duration", ["secs","nanos"])`),
  `GenericSignature` (local newtype struct around `Vec<u8>`), `AccountAddress` (local newtype `Value`).
- leaf chains: `ObjectID` = 2 (ObjectID > AccountAddress), digest wrappers = 2 (e.g. ObjectDigest > Digest),
  `ChainIdentifier` = 3, `MoveObjectType` = 2 + contents, `ECMHLiveObjectSetDigest` = struct + Digest = 2.

Only `TypeTag` / `TypeInput` recurse without bound, so this only matters there: `TypeTag` enum = 1,
`StructTag` struct = 1, plus `AccountAddress` = 1 at the leaf of every struct. E.g. top-level
`bcs::from_bytes::<TypeTag>` of `Vector^k(Bool)` is accepted iff `k + 1 <= 500`; a chain of nested
single-param structs of length m reaches depth `2m + 1`.

### bool / option
`de.rs:217-225`: bool byte must be 0 or 1 else `ExpectedBoolean`. `de.rs:571-582`: option tag must be 0 or
1 else `ExpectedOption`.

### map canonical ordering (`de.rs:751-775`)
```rust
let (key_value, key_bytes) = self.de.next_key_seed(seed)?;
if let Some(previous_key_bytes) = &self.previous_key_bytes {
    if previous_key_bytes.as_ref() >= key_bytes.as_ref() { return Err(Error::NonCanonicalMap); }
}
```
`key_bytes` is the exact slice of input consumed by the key (`de.rs:379-388`). Comparison is plain
lexicographic `&[u8]` ordering, **strict** (duplicates rejected). For `String` keys that includes the uleb
length prefix, so `"b"` (01 62) sorts before `"aa"` (02 61 61), and length-128+ prefixes compare bytewise
(`80 02` (256) < `81 01` (129)). Only `deserialize_map` does this. `BTreeSet`/`HashSet` go through
`deserialize_seq`: **no ordering or duplicate check** (none of the YAML types use sets, FYI).

### strings
`de.rs:411-414`: `std::str::from_utf8(slice).map_err(|_| Error::Utf8)`. Strict UTF-8 (no surrogates, no
overlongs, max U+10FFFF). NUL and any other scalar are fine.

### enum variant index
uleb128-u32 as above, then serde-derive's identifier visitor: index >= variant count -> `invalid value`
error. There are no `#[serde(skip)]`/`other` variants in the listed types.

### char / f32 / f64 / unit / any
`de.rs:522-541`: `deserialize_f32`, `deserialize_f64`, `deserialize_char` -> `Err(NotSupported)`.
`deserialize_any` / `deserialize_ignored_any` -> `NotSupported`. `deserialize_unit` consumes 0 bytes
(`de.rs:584-589`). `i8..i128` are supported as two's complement LE. `deserialize_identifier` ->
`deserialize_bytes` (unused by derive for enums; variant goes through `variant_seed`).
`usize` (`ExecutionFailure.command: Option<usize>`) deserializes as u64; on 64-bit every value fits.

`Vec<u8>` without `serde_as(Bytes)` (e.g. `CallArg::Pure`, `GenericSignature`'s inner vec,
`version_specific_data`) goes element-by-element through `deserialize_seq`; wire format identical to BYTES.

---------------------------------------------------------------------------------------------------

## 2. move-core-types

`external-crates/move/crates/move-core-types/src/`

### Identifier (`identifier.rs:44-76, 99-106, 110-117, 165-171`)
Hand-written: `serde_with::DisplayFromStr::deserialize_as(deserializer)` -> `deserialize_str` ->
`Identifier::from_str` -> `Identifier::new` -> `is_valid`:
```rust
pub const fn is_valid_identifier_char(c: char) -> bool { matches!(c, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9') }
pub const fn is_valid(s: &str) -> bool {
    let b = s.as_bytes();
    match b {
        [b'a'..=b'z', ..] | [b'A'..=b'Z', ..] => all_bytes_valid(b, 1),
        [b'_', ..] if b.len() > 1 => all_bytes_valid(b, 1),
        _ => false,
    }
}
```
i.e. regex `^([a-zA-Z][a-zA-Z0-9_]*|_[a-zA-Z0-9_]+)$`. Empty rejected, `"_"` rejected, no `<SELF>` special
case, no length limit (other than bcs). UTF-8 check happens first. **No newtype-struct call => depth 0 and no
YAML entry** (YAML shows `STR`).

Where Identifier appears among YAML types: `StructTag.module`, `StructTag.name`, `ModuleId.name`
(`language_storage.rs:169-177, 300-305`), `Event.transaction_module` (`ST/event.rs:108`). Transitively:
every `TypeTag::Struct`, `MoveObjectType_::Other/Coin/BalanceAccumulatorField`, `Event.type_`,
`AccumulatorAddress.ty`, `WithdrawalTypeArg::Balance`, `MoveLocation.module`.

### TypeTag / StructTag (`language_storage.rs:31-59, 169-177`)
Plain `#[derive(Deserialize)]`; variants in YAML order; `Vector(Box<TypeTag>)`, `Struct(Box<StructTag>)`.
`Box` is transparent to serde (no bytes, no depth). **No custom depth limit** - `grep MAX_TYPE_TAG_NESTING`
finds nothing in the crate; only the bcs 500 limit applies. `type_params` is renamed `type_args` (JSON only).
`Signer` (index 5) is accepted.

### AccountAddress (`account_address.rs:285-305`)
Binary path: `#[derive(Deserialize)] #[serde(rename = "AccountAddress")] struct Value([u8; 32]);` ->
newtype struct (depth +1) around a 32-tuple: exactly 32 raw bytes, no length prefix, no validation.

### ModuleId (`language_storage.rs:300-305`): derived struct `{address: AccountAddress, name: Identifier}`.

---------------------------------------------------------------------------------------------------

## 3. TypeInput / StructInput (`ST/type_input.rs:14-53`)

Plain derives. `StructInput { address: AccountAddress, module: String, name: String, type_params: Vec<TypeInput> }`
- module/name are plain `String` (UTF-8 only, NOT identifier-validated, may be empty).
`TypeInput` has the same 11 variants / order as TypeTag with `Box`ed recursion. No custom deserialize and no
depth limit besides bcs 500 (`type_input_validity_check`, `ST/transaction.rs:233+`, is a later
protocol-config check, not deser).

---------------------------------------------------------------------------------------------------

## 4. Digests (`ST/digests.rs`)

```rust
#[serde_as] #[derive(... Serialize, Deserialize ...)]
pub struct Digest(#[serde_as(as = "Readable<Base58, Bytes>")] [u8; 32]);      // digests.rs:20-28
```
Binary path = `serde_with::Bytes` for `[u8; N]` (serde_with-3.15.1 `src/de/impls.rs:1467-1508`):
`deserializer.deserialize_bytes(ArrayVisitor::<N>)`, `visit_bytes` does
`v.try_into().map_err(|_| DeError::invalid_length(v.len(), &self))`. So the wire is
**uleb length that must be exactly 32 (single byte 0x20) followed by 32 bytes**; any other length rejects.
No content validation.

All wrappers are plain derived newtype structs over `Digest` (depth 2 total, identical bytes):
`CheckpointDigest` (:282), `CheckpointContentsDigest` (:379), `TransactionDigest` (:497),
`TransactionEffectsDigest` (:626), `TransactionEventsDigest` (:728), `EffectsAuxDataDigest` (:798),
`ObjectDigest` (:856), `ConsensusCommitDigest` (:991), `AdditionalConsensusStateDigest` (:1046),
`CheckpointArtifactsDigest` (:1072). No differences between them at deser. (`SenderSignedDataDigest`,
`ZKLoginInputsDigest` have no serde.)
`ChainIdentifier(CheckpointDigest)` (:162) - derived newtype; depth 3; same 33 bytes.
`ECMHLiveObjectSetDigest { digest: Digest }` (`ST/messages_checkpoint.rs:108-112`) - struct, same 33 bytes.

---------------------------------------------------------------------------------------------------

## 5. SenderSignedData

`ST/transaction.rs:3652-3653`: `pub struct SenderSignedData(SizeOneVec<SenderSignedTransaction>);` (derived
newtype struct, depth +1).

`ST/base_types.rs:1867-1871, 1908-1920`:
```rust
#[derive(Debug, Deserialize, ...)] #[serde(try_from = "Vec<T>")]
pub struct SizeOneVec<T> { e: T }
impl<T> TryFrom<Vec<T>> for SizeOneVec<T> { fn try_from(mut v: Vec<T>) -> ... { if v.len() != 1 { Err(anyhow!("Expected a vec of size 1")) } else { ... } } }
```
So the whole `Vec` is parsed first, then rejected unless `len == 1`. Net rule: uleb length must be 1.

`SenderSignedTransaction` has a hand-written Deserialize (`ST/transaction.rs:3688-3714`): derives a local
`struct SignedTxn { intent_message: IntentMessage<TransactionData>, tx_signatures: Vec<GenericSignature> }`
(renamed "SenderSignedTransaction", depth +1) and then:
```rust
if intent_message.intent != Intent::sui_transaction() { return Err(serde::de::Error::custom("invalid Intent for Transaction")); }
```
`Intent::sui_transaction()` = `{TransactionData(0), V0(0), Sui(0)}` (`crates/shared-crypto/src/intent.rs:121-127`).
So inside a SenderSignedData the three intent bytes must be `00 00 00`. No constraint on `tx_signatures`
count (0 is fine) at deser.

---------------------------------------------------------------------------------------------------

## 6. Signatures

### 6.1 GenericSignature (`ST/signature.rs:299-316`, `239-269`)
Binary path:
```rust
#[derive(serde::Deserialize)] struct GenericSignature(Vec<u8>);            // newtype struct (depth+1), seq of u8
let data = GenericSignature::deserialize(deserializer)?;
Self::from_bytes(&data.0).map_err(|e| Error::custom(e.to_string()))
```
`from_bytes` (:239-269): first byte through `SignatureScheme::from_flag_byte` (`ST/crypto.rs:1744-1755`):

| flag | scheme | handling |
|---|---|---|
| 0x00 | ED25519 | `Signature::from_bytes` |
| 0x01 | Secp256k1 | `Signature::from_bytes` |
| 0x02 | Secp256r1 | `Signature::from_bytes` |
| 0x03 | MultiSig | `MultiSig::from_bytes`, **on any error fall back to `MultiSigLegacy::from_bytes`** |
| 0x04 | BLS12381 | known flag but `_ => Err(InvalidInput)` -> rejected |
| 0x05 | ZkLoginAuthenticator | `ZkLoginAuthenticator::from_bytes` |
| 0x06 | PasskeyAuthenticator | `PasskeyAuthenticator::from_bytes` |
| other / empty | | rejected |

ALL of this happens at deser time. The same Deserialize impl is used everywhere a `GenericSignature`
appears: `SenderSignedTransaction.tx_signatures`, `CheckpointContentsV1.user_signatures`
(`Vec<Vec<GenericSignature>>`), `CheckpointTransactionContents.user_signatures`
(`Vec<(GenericSignature, Option<SequenceNumber>)>`), `FullCheckpointContents.user_signatures`.

### 6.2 Simple signatures (`ST/crypto.rs:801-818, 836-947`)
`Signature::from_bytes`: dispatch on first byte then `XxxSuiSignature::from_bytes` which ONLY checks length:
```rust
if bytes.len() != Self::LENGTH { return Err(FastCryptoError::InputLengthWrong(Self::LENGTH)); }
```
Layout `flag || sig || pk`:
- 0x00 Ed25519: 1 + 64 + 32 = **97** bytes
- 0x01 Secp256k1: 1 + 64 + 33 = **98** bytes
- 0x02 Secp256r1: 1 + 64 + 33 = **98** bytes
No curve / scalar validation here; that is verify-time (`get_verification_inputs`, crypto.rs:~975).

Standalone `Signature` Deserialize (`ST/crypto.rs:743-759`) uses `Bytes::deserialize_as` (uleb len + bytes)
then the same `from_bytes`. It only appears nested inside ZkLogin / Passkey payloads.

### 6.3 MultiSig (flag 0x03) (`ST/multisig.rs:48-64, 392-402, 448-459, 493-499, 552-573`)
`bytes[1..]` is `bcs::from_bytes::<MultiSig>` (fresh depth budget, no trailing bytes), struct
`{ sigs: Vec<CompressedSignature>, bitmap: u16, multisig_pk: MultiSigPublicKey{ pk_map: Vec<(PublicKey,u8)>, threshold: u16 } }`
(`bytes: OnceCell` is `#[serde(skip)]`). Then `init_and_validate`:
```rust
if self.sigs.len() > self.multisig_pk.pk_map.len() || self.sigs.is_empty() || self.bitmap > MAX_BITMAP_VALUE /*0b1111111111*/ { Err }
self.multisig_pk.validate()?
// validate(): threshold == 0 || pk_map.is_empty() || pk_map.len() > 10 || any weight == 0
//             || sum(weights as u16) < threshold || any duplicate pk (derived PartialEq on PublicKey: variant + bytes)
```
NOT checked at deser: bitmap popcount vs `sigs.len()`, bitmap bits >= pk_map.len(), sig/pk scheme
agreement, contents of ZkLogin / Passkey compressed sigs (parsed at verify: multisig.rs:252, 268).

`CompressedSignature` (`ST/crypto.rs:1758-1771`), derived enum:
0 Ed25519 `[u8;64]`, 1 Secp256k1 `[u8;64]`, 2 Secp256r1 `[u8;64]` (all `BytesRepresentation`: raw N bytes,
no prefix, depth 0), 3 `ZkLogin(ZkLoginAuthenticatorAsBytes(Vec<u8>))`, **4 `Passkey(PasskeyAuthenticatorAsBytes(Vec<u8>))` - missing from YAML**.

`PublicKey` (`ST/crypto.rs:263-270`), derived enum: 0 Ed25519 `[u8;32]`, 1 Secp256k1 `[u8;33]`,
2 Secp256r1 `[u8;33]`, 3 `ZkLogin(ZkLoginPublicIdentifier(Vec<u8>))`, **4 `Passkey([u8;33])` - missing from YAML**.
Raw bytes, NO point validation at deser. `ZkLoginPublicIdentifier` is an unvalidated `Vec<u8>` newtype at
deser (`ST/crypto.rs:274-275`); its structure is only checked at verify when
`validate_zklogin_public_identifier` is on. `ZkLoginAuthenticatorAsBytes` likewise opaque.

`BytesRepresentation<N>` binary deser: `FC:fastcrypto/src/serde_helpers.rs:232-258` ->
`SerializationHelper<N>` = `#[serde(transparent)] (#[serde_as(as = "[_; N]")] [u8; N])` (:61-64) -> N-tuple of u8.

### 6.4 MultiSigLegacy fallback (flag 0x03) (`ST/multisig_legacy.rs:42-56, 207-217, 231-243, 248-290, 143-162`)
Tried only if 6.3 fails (bcs error OR validation error). `bcs::from_bytes::<MultiSigLegacy>(&bytes[1..])`:
```
sigs: Vec<CompressedSignature>
bitmap: BYTES  -> SuiBitmap -> deserialize_sui_bitmap (see section 7; cardinality <= 150)
multisig_pk: MultiSigPublicKeyLegacy { pk_map: Vec<(String, u8)>, threshold: u16 }
```
Each pk String goes through `PublicKey::decode_base64` (`ST/crypto.rs:352-383`):
`base64ct::Base64::decode_vec` (standard alphabet, padded, strict/canonical - FC:fastcrypto/src/encoding.rs:190-193),
first byte = flag in {0x00, 0x01, 0x02, 0x06}, remainder through
`Ed25519PublicKey::from_bytes` (ed25519_consensus `VerificationKey::try_from`: 32 bytes that decompress),
`Secp256k1PublicKey::from_bytes` (rust-secp256k1 `PublicKey::from_slice`: 33- or 65-byte valid point) or
`Secp256r1PublicKey::from_bytes` (p256 `VerifyingKey::try_from`, SEC1; see 6.6). ZkLogin flag is NOT accepted here.
Then `validate()`: `bitmap_to_u16` requires every set bit index `< 10`; `sigs.len() <= pk_map.len()`;
`!sigs.is_empty()`; then the same `MultiSigPublicKey::validate` as 6.3.
=> exact accept/reject parity on this path needs real curve code for three curves. I did not audit the
three external crates' edge cases (hybrid 0x06/0x07 secp256k1 tags, non-canonical ed25519 y, etc.).

### 6.5 ZkLoginAuthenticator (flag 0x05) (`ST/zk_login_authenticator.rs:30-38, 255-268`)
`bcs::from_bytes::<ZkLoginAuthenticator>(&bytes[1..])` then `zk_login.inputs.init()?`. BCS layout:
```
inputs: ZkLoginInputs {                                   FC:fastcrypto-zkp/src/bn254/zk_login.rs:491-500
  proof_points: ZkLoginProof { a: Vec<Fq>, b: Vec<Vec<Fq>>, c: Vec<Fq> },       (:598-603) lengths NOT checked at deser
  iss_base64_details: Claim { value: String, index_mod_4: u8 },                 (:462-467)
  header_base64: String,
  address_seed: Bn254FrElement,
  #[serde(skip)] jwt_details
}
max_epoch: u64
user_signature: Signature          // BYTES, 97/98/98 with flag 0/1/2 as in 6.2
#[serde(skip)] bytes
```
`Bn254FqElement` / `Bn254FrElement` (`FC:fastcrypto-zkp/src/zk_login_utils.rs:24-35, 57-65, 91-101, 122-130`):
bcs string -> `Fq::from_str` / `Fr::from_str` (ark-ff 0.4.2 `src/fields/models/fp/mod.rs:638-683`):
empty -> Err; exactly `"0"` -> ok; otherwise every char must be an ASCII digit (`c.to_digit(10)`) and the
first digit must not be 0. The accumulation is done **in the field**, so the final `is_geq_modulus()` check
can never fire: the accept set is the regex `^(0|[1-9][0-9]*)$` of ANY length (values >= modulus are silently
reduced). The `to_bytes_be().try_into::<[u8;32]>()` always succeeds.

`init()` -> `JWTDetails::new(&header_base64, &iss_base64_details)` (zk_login.rs:477-487):
1. `JWTHeader::new(header_base64)` (`FC:fastcrypto/src/jwt_utils.rs:57-77`):
   `base64ct::Base64UrlUnpadded::decode_vec` (strict: url alphabet, no `=`, len%4 != 1, canonical trailing
   bits) -> `serde_json::from_slice::<JWTHeader>` where `struct JWTHeader { alg: String, kid: String, typ: Option<String> }`
   (unknown fields ignored, duplicate known fields error, trailing non-whitespace errors) -> `alg == "RS256"`.
2. `decode_base64_url(&claim.value, &claim.index_mod_4)` (zk_login.rs:648-695): `value.len() >= 2`; every
   char in the url-safe alphabet; `index_mod_4` in {0,1,2} (drop 0/2/4 leading bits);
   `last_char_offset = (i + s.len() as u8 - 1) % 4` must be 3/2/1 (drop 0/2/4 trailing bits), 0 -> Err;
   remaining bit count % 8 == 0; bytes must be UTF-8.
   NOTE `i + s.len() as u8 - 1` is u8 arithmetic: in debug/test builds (overflow-checks on) it **panics** for
   e.g. `len % 256 == 0 && i == 0`, or `i + len%256 > 255`; sui's `[profile.release]` (Cargo.toml:234-242) leaves
   overflow-checks off so it wraps, and since 256 % 4 == 0 the wrapped result equals the exact
   `(i + len - 1) mod 4`.
3. `verify_extended_claim(&ext_claim, "iss")` (zk_login.rs:624-645): last char must be `}` or `,`;
   `serde_json::from_str::<Value>("{" + ext[..len-1] + "}")` must parse to an object that has key `"iss"`
   with a JSON string value (duplicate keys: last wins).
No length limits (MAX_HEADER_LEN etc.) and no proof-point validation at deser.

### 6.6 PasskeyAuthenticator (flag 0x06) (`ST/passkey_authenticator.rs:68-74, 77-130, 133-145, 276-288`)
`bcs::from_bytes::<PasskeyAuthenticator>(&bytes[1..])`, hand-written Deserialize =
`RawPasskeyAuthenticator { authenticator_data: Vec<u8>, client_data_json: String, user_signature: Signature }`
then `TryFrom`:
1. `serde_json::from_str::<CollectedClientData>(&client_data_json)` (passkey-types 0.4.0
   `src/webauthn/attestation.rs:577-616`): camelCase; required `type: ClientDataType`
   ("webauthn.create" | "webauthn.get" | "payment.get"), required `challenge: String`, required
   `origin: String`, optional `crossOrigin: Option<bool>` (`#[serde(default)]`), plus two `#[serde(flatten)]`
   catch-alls (`extra_data: ()`, `unknown_keys: IndexMap<String, Value>`).
2. `ty == ClientDataType::Get`.
3. `Base64UrlUnpadded::decode_vec(challenge)` must decode to exactly 32 bytes.
4. `user_signature.scheme() == Secp256r1` (so the nested Signature must be flag 0x02, 98 bytes).
5. `Secp256r1PublicKey::from_bytes(pk 33 bytes)` (`FC:fastcrypto/src/secp256r1/mod.rs:233-247`) ->
   p256 `VerifyingKey::try_from(&[u8])` = SEC1 decode. sec1 0.7.1 `point.rs:88-108, 479-488` accepts tags
   0x02, 0x03 **and 0x05 (compact)** for a 33-byte input; primeorder 0.13.0 `affine.rs:177-189` handles
   `Coordinates::Compact { x } => Self::decompact(x)`. Requires x < p and x^3 - 3x + b to be a square.
6. `Secp256r1Signature::from_bytes(sig 64 bytes)` (`:311-325`) -> `p256::ecdsa::Signature::try_from`: r and s
   each in [1, n-1] (comment there: "fails if either r or s are zero"); no low-s requirement at parse.

=> exact parity for passkey requires P-256 field sqrt + a serde_json-compatible JSON parser.

---------------------------------------------------------------------------------------------------

## 7. Authority types

### AuthorityPublicKeyBytes (`ST/crypto.rs:433-451`)
`#[serde_as(as = "Readable<Base64, Bytes>")] pub [u8; AuthorityPublicKey::LENGTH]` with
`AuthorityPublicKey = BLS12381PublicKey` from `fastcrypto::bls12381::min_sig` (`ST/crypto.rs:13, 66`) ->
LENGTH = 96 (G2). Derived newtype struct (depth +1) + serde_with `Bytes` for `[u8; N]`:
**uleb length must equal 96 (`0x60`) then 96 bytes**. No point validation.

### AuthorityQuorumSignInfo (`ST/crypto.rs:1234-1243`)
```rust
pub epoch: EpochId,
pub signature: AggregateAuthoritySignature,          // = BLS12381AggregateSignature (min_sig, G1, 48 bytes)
#[serde_as(as = "SuiBitmap")] pub signers_map: RoaringBitmap,
```
**signature**: `serialize_deserialize_with_to_from_bytes!` (`FC:fastcrypto/src/serde_helpers.rs:127-157`):
reads `SerializationHelper<48>` (48 raw bytes) then `BLS12381AggregateSignature::from_bytes`
(`FC:fastcrypto/src/bls12381/mod.rs:564-574`): `blst::Signature::from_bytes(bytes)` - comment says "does NOT
validate the signature" (no subgroup check) but blst still *decodes* the point. blst-0.3.16
`src/lib.rs:1474-1492`: for a 48-byte input requires `sig_in[0] & 0x80 != 0`, then `blst_p1_deserialize` ->
`POINTonE1_Uncompress_Z` (`blst/src/e1.c:236-291`):
- bit 0x80 (compressed) must be set;
- if bit 0x40 (infinity) set: `(in0 & 0x3f) == 0` and the other 47 bytes zero, i.e. exactly `c0 00..00`
  (accepted as infinity); anything else -> BAD_ENCODING;
- else x = big-endian 381-bit value with top 3 bits cleared; must be `< p`; `x^3 + 4` must be a square in
  Fp (`sqrt_fp`), else POINT_NOT_ON_CURVE; sign bit 0x20 is free; and `x == 0` is rejected
  (`return vec_is_zero(out->X) ? BLST_POINT_NOT_IN_GROUP : BLST_SUCCESS`).
No subgroup check. So an independent parser needs BLS12-381 Fp arithmetic (Legendre / sqrt) to match.

**signers_map**: `SuiBitmap` (`ST/sui_serde.rs:333-368`): `Bytes` (uleb len + bytes) then
`deserialize_sui_bitmap`:
```rust
let orig_bitmap = roaring::RoaringBitmap::deserialize_from(bytes)?;
if orig_bitmap.len() > MAX_VALIDATOR_COUNT /* 150, ST/governance.rs:23 */ { return Err(...) }
// then rebuilds deduplicated (cannot fail)
```
`deserialize_from` (roaring-0.11.4 `src/bitmap/serialization.rs:174-176, 205-325`), reading from `&[u8]`:
- u32 LE cookie. `12346` (SERIAL_COOKIE_NO_RUNCONTAINER): next u32 LE = container count `size`, offsets
  always present. `(cookie as u16) == 12347` (SERIAL_COOKIE): `size = (cookie >> 16) + 1`, run-flag bitmap of
  `ceil(size/8)` bytes follows, offsets present iff `size >= 4`. Otherwise "unknown cookie value".
- `size > 65536` -> error.
- `size` descriptions of 4 bytes: `key: u16 LE`, `cardinality-1: u16 LE`. Keys must be strictly increasing
  ("container keys are not sorted").
- if offsets present: `size * 4` bytes read and **ignored** (not validated).
- per container: run container (flag bit set): `runs: u16` must be != 0; `runs` x (start u16, len u16);
  `start + len` must not overflow u16; each next `start` must be `> prev_end + 1` (saturating) else error;
  the description cardinality is NOT cross-checked. Else if `cardinality <= 4096`: `cardinality` u16 LE
  values that must be strictly increasing (`ArrayStore::try_from`, `store/array_store/mod.rs:395-413`).
  Else: 8192 bytes of bitmap whose popcount must equal `cardinality` (`store/bitmap_store.rs:33-40`).
- any short read -> error. **Bytes left over after the last container are ignored** (no end check), so
  trailing garbage inside the BYTES field is accepted.
- cardinality (sum over containers; for runs sum of `len+1`) must be <= 150.
- empty bitmap = `3a 30 00 00 00 00 00 00` is fine. Values themselves may be any u32.
No other checks at deser (epoch, signer indices vs committee, stake are verify-time).

### EmptySignInfo (`ST/crypto.rs:1095-1096`): `pub struct EmptySignInfo {}` - derived, zero bytes, depth +1.

---------------------------------------------------------------------------------------------------

## 8. Intent / IntentMessage (`crates/shared-crypto/src/intent.rs:16-20, 33-40, 53-66, 79-85, 162-166`)

All three enums are `#[derive(Serialize_repr, Deserialize_repr)] #[repr(u8)]`: a plain u8 on the wire (not a
uleb variant index, no container), unknown value -> error.
- `IntentScope`: 0 TransactionData, 1 TransactionEffects, 2 CheckpointSummary, 3 PersonalMessage,
  4 SenderSignedTransaction, 5 ProofOfPossession, 6 HeaderDigest, 7 BridgeEventUnused, 8 ConsensusBlock,
  9 DiscoveryPeers -> accepted **0..=9**
- `IntentVersion`: V0 = 0 -> accepted **0 only**
- `AppId`: Sui = 0, Narwhal = 1, Consensus = 2 -> accepted **0..=2**
`Intent` and `IntentMessage<T>` are derived structs (field order scope, version, app_id; intent, value).
A standalone `IntentMessage<TransactionData>` accepts any valid triple; inside `SenderSignedTransaction` it
must be (0,0,0) (section 5).

---------------------------------------------------------------------------------------------------

## 9. Objects and packages

### MovePackage (`ST/move_package.rs:100-124`)
Derived with `#[serde_as(as = "BTreeMap<_, Bytes>")] module_map: BTreeMap<String, Vec<u8>>`,
`type_origin_table: Vec<TypeOrigin>`, `linkage_table: BTreeMap<ObjectID, UpgradeInfo>`. Both maps go through
`deserialize_map` -> strict increasing serialized-key-bytes rule (section 1; for `module_map` that is
length-prefix-then-bytes order, not string order). Keys are plain `String` (not Identifier). No other
validation at deser (no module bytecode parsing, no size limits, id/version unchecked).
`TypeOrigin { module_name: String, datatype_name: String, package: ObjectID }` (:79-88; `struct_name` alias is
JSON-only). `UpgradeInfo { upgraded_id, upgraded_version }` (:91-97). All derived.

### MoveObject (`ST/object.rs:53-66`)
Derived: `type_: MoveObjectType, has_public_transfer: bool, version: SequenceNumber, #[serde_as(as="Bytes")] contents: Vec<u8>`.
No deser-time validation at all (contents may be empty / shorter than 32 bytes, has_public_transfer is not
cross-checked, no size limit). Later accessors such as `id()` would panic on short contents - not deser.

### MoveObjectType (`ST/base_types.rs:234-259`)
Derived newtype `MoveObjectType(MoveObjectType_)` + derived enum (Other(StructTag), GasCoin, StakedSui,
Coin(TypeTag), SuiBalanceAccumulatorField, BalanceAccumulatorField(TypeTag)). No custom serde and no
canonicalisation on deser: `Other(0x2::coin::Coin<0x2::sui::SUI>)` is accepted as-is.

### Object (`ST/object.rs:1084-1101`)
`#[serde(from = "ObjectInner")] pub struct Object(Arc<ObjectInner>)`; `ObjectInner` is
`#[serde(rename = "Object")]` derived struct `{data: Data, owner: Owner, previous_transaction, storage_rebate}`.
Depth +1 only, no validation. `Data` derived enum (Move, Package).

### Owner / Party / ObjectPermissions (`ST/object.rs:562, 637-647, 712-717, 817-824, 876-900, 933-970`)
`Owner` derived; variants as in YAML. `Party` is `#[serde(try_from = "RawPartySerde", into = "RawPartySerde")]`:
```rust
struct RawPartySerde { default_permissions: ObjectPermissions, members: Vec<(SuiAddress, ObjectPermissions)> }
```
`ObjectPermissions` is `#[serde(try_from = "u64", into = "u64")]` (the YAML `U64`s) -> `ObjectPermissions::new(bits)`:
```rust
if bits & !Self::ALL_BITS != 0 { return None; }                                  // ALL_BITS = 0x7f
let has_mutable_perm = bits & Self::MUTABLE_PERMISSION_BITS != 0;                // 0x04|0x08|0x10|0x20|0x40 = 0x7c
let has_mutable_usage = bits & (ObjectPermission::MutableUsage as u64) != 0;     // 0x02
if has_mutable_perm && !has_mutable_usage { return None; }
```
Bits: ImmutableUsage 0x01, MutableUsage 0x02, Write 0x04, Delete 0x08, InternalTransfer 0x10,
PublicTransfer 0x20, Wrap 0x40 (:515-523). Applies to `default_permissions` and every member value.
`TryFrom<RawPartySerde> for Party` (:882-899):
```rust
for window in raw.members.windows(2) { match window[0].0.cmp(&window[1].0) { Less => continue, Equal => Err("duplicate Party member address"), Greater => Err("Party members must be sorted") } }
Self::new(default, members).ok_or("invalid Party: violates canonical representation")
// new() returns None iff default_permissions == NONE(0) && members.len() == 1 && that value == ALL(0x7f)
```
SuiAddress order = lexicographic on the 32 bytes (derived Ord on `[u8;32]`). Empty members is fine.

---------------------------------------------------------------------------------------------------

## 10. Effects

All plain derives, no custom deser: `TransactionEffects` (`ST/effects/mod.rs:54-60`),
`TransactionEffectsV1`, `TransactionEffectsV2` (`ST/effects/effects_v2.rs:29-67`), `EffectsObjectChange`
(`ST/effects/object_change.rs:18-30`), `ObjectIn` (:75-80), `AccumulatorOperation` (:82-88),
`AccumulatorAddress` (:99-103), `AccumulatorWriteV1` (:111-118), `ObjectOut` (:210-221),
`UnchangedConsensusKind` (`effects_v2.rs:786-799`), `IDOperation`.
No deser check that `gas_object_index < changed_objects.len()` (the accessor `gas_object()` indexes and
would panic, effects_v2.rs:440-462), nor uniqueness/sortedness of `changed_objects`.

Exception: `AccumulatorValue` (`object_change.rs:90-96`):
```rust
Integer(u64), IntegerTuple(u64, u64), EventDigest(NonEmpty<(u64, Digest)>)
```
`nonempty` 0.9.0 (`src/lib.rs:119-125`): `#[serde(try_from = "Vec<T>")]` -> **empty sequence rejected**.
The inner digest is the bare `Digest` (0x20 + 32 bytes).

`ExecutionStatus` / `ExecutionFailure` (`ST/execution_status.rs:19-30, 519`):
```rust
pub enum ExecutionStatus { Success, Failure(ExecutionFailure) }
pub struct ExecutionFailure { pub error: ExecutionErrorKind, pub command: Option<CommandIndex> }   // CommandIndex = usize
```
Derived, matches YAML (newtype variant around a struct => one extra depth level vs. the historical struct
variant; bytes identical). `usize` reads a u64.
`ExecutionErrorKind`: derived, 42 variants (0..=41) matching the YAML; contains `MoveLocation`
(`module: ModuleId` => **Identifier-validated name**; `function: u16, instruction: u16,
function_name: Option<String>` plain), `MoveLocationOpt(pub Option<MoveLocation>)` (:313-314) derived
newtype, `CongestedObjects(pub Vec<ObjectID>)` (:78-79) derived newtype, no dedup/order/nonempty check.
`AddressDeniedForCoin.coin_type` / `CoinTypeGlobalPause.coin_type` are plain Strings.

---------------------------------------------------------------------------------------------------

## 11. Checkpoints

- `CheckpointSummary` (`ST/messages_checkpoint.rs:327-359`): derived; `version_specific_data: Vec<u8>` is an
  unparsed byte seq at deser.
- `EndOfEpochData` (:302-325): `#[serde(rename_all = "camelCase")]` -> YAML field names
  `nextEpochCommittee`, `nextEpochProtocolVersion`, `epochCommitments` (names don't hit the wire). The
  `serde_as` adapters are `Readable<.., _>`: binary path = plain `(AuthorityPublicKeyBytes, u64)` and plain
  `ProtocolVersion(u64)` (`crates/sui-protocol-config/src/lib.rs:359-360`, derived, no range check).
  No sortedness / dup check on the committee.
- `CheckpointCommitment` (:284-288) derived enum; `ECMHLiveObjectSetDigest { digest: Digest }`.
- `CheckpointContents` (:594-598) derived enum V1/V2. `CheckpointContentsV1` (:600-607) and
  `CheckpointContentsV2` (:609-615) have `#[serde(skip)] digest: OnceCell<..>` (no bytes).
  **`transactions.len() == user_signatures.len()` is NOT enforced at deser** - only `assert_eq!` in
  constructors (:629, :645, :672).
- `CheckpointTransactionContents { digest: ExecutionDigests, user_signatures: Vec<(GenericSignature, Option<SequenceNumber>)> }` (:617-622).
- `FullCheckpointContents { transactions: Vec<ExecutionData>, user_signatures: Vec<Vec<GenericSignature>> }`
  (:1031-1038) derived, no length invariant at deser.
- `ExecutionData { transaction: Transaction, effects: TransactionEffects }` (`ST/base_types.rs:1041-1045`).
- `CheckpointData` / `CheckpointTransaction` (`ST/full_checkpoint_content.rs:20-25, 105-117`) derived.
- `CertifiedCheckpointSummary = Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>` (:508).
All GenericSignatures inside are fully validated as in section 6; every nested
`Envelope<SenderSignedData, EmptySignInfo>` applies section 5.

---------------------------------------------------------------------------------------------------

## 12. Transaction input types (`ST/transaction.rs`)

All plain derives, match YAML:
- `TransactionExpiration` (:2319-2349): `None`, `Epoch(u64)`, `ValidDuring { min_epoch, max_epoch, min_timestamp, max_timestamp: Option<u64>, chain: ChainIdentifier, nonce: u32 }`.
- `CallArg` (:115-125), `ObjectArg` (:141-156), `Reservation::MaxAmountU64(u64)` (:158-162),
  `WithdrawalTypeArg::Balance(TypeTag)` (:164-167), `FundsWithdrawalArg` (:187-195),
  `WithdrawFrom { Sender, Sponsor }` (:197-204), `SharedObjectMutability { Immutable, Mutable, NonExclusiveWrite }` (:4586-4595).
- `ObjectArg::SharedObject.mutability`: **no custom serde**. Comment at :150-152: "this used to be a bool, but
  because true/false encode to 0x00/0x01, we are able to be backward compatible". It is an ordinary enum
  variant index: single byte 0x00 / 0x01 / 0x02 (uleb, so `80 00` is rejected; 0x03+ rejected).
- `Reservation` amount 0 is accepted at deser (rejected later in `accumulate_funds_withdrawals`).
- `TransactionKind` (:458-492) 11 variants and `EndOfEpochTransactionKind` (:495-510) 13 variants, as YAML.
- `StoredExecutionTimeObservations::V1(Vec<(ExecutionTimeObservationKey, Vec<(AuthorityName, Duration)>)>)` (:350-353):
  `AuthorityName` = AuthorityPublicKeyBytes (0x60 + 96 bytes). `std::time::Duration` via serde_core-1.0.228
  `src/de/impls.rs:2129-2265`: struct (depth +1) `secs: u64, nanos: u32`, then
  `check_overflow`: `secs.checked_add((nanos / 1_000_000_000) as u64)` must be `Some` else
  "overflow deserializing Duration". nanos >= 1e9 is otherwise ACCEPTED (and would re-serialize differently).
- `ExecutionTimeObservationKey::MoveEntryPoint { package, module: String, function: String, type_arguments: Vec<TypeInput> }`
  (`ST/execution.rs:275-295`) plain strings.

---------------------------------------------------------------------------------------------------

## 13. Envelope (`ST/message_envelope.rs:32-56`)

```rust
#[derive(Clone, Debug, Eq, Serialize, Deserialize)] #[serde(remote = "Envelope")]
pub struct Envelope<T: Message, S> { #[serde(skip)] digest: OnceCell<T::DigestType>, data: T, auth_signature: S }
```
The real `Deserialize` impl just wraps the deserializer in `serde_name::DeserializeNameAdapter` to set the
struct name to `type_name::<Self>()` (that is why the YAML keys are the long type names). Wire = `data` then
`auth_signature`; `digest` is skipped (0 bytes); depth +1. `EmptySignInfo` = 0 bytes.

---------------------------------------------------------------------------------------------------

## 14. Inventory of non-derived / adapted Deserialize impls relevant to the YAML list

(`grep "impl<'de>.*Deserialize|serde(try_from|deserialize_with|serde(from|serde(with|serde_as"` over
`crates/sui-types/src`, `crates/shared-crypto/src`, plus dependencies that the YAML types pull in.)

| Type | Where | Extra validation vs YAML |
|---|---|---|
| `Identifier` | move-core-types identifier.rs:99-106 | identifier grammar; depth 0 |
| `AccountAddress` | account_address.rs:285-305 | none (32 raw bytes) |
| `Digest` (+ all wrappers) | digests.rs:20-28 | BYTES length must be exactly 32 |
| `AuthorityPublicKeyBytes` | crypto.rs:433-451 | BYTES length must be exactly 96 |
| `AuthorityQuorumSignInfo.signature` | fastcrypto macro | valid compressed G1 encoding (on curve, x != 0, or exact infinity) |
| `AuthorityQuorumSignInfo.signers_map` | sui_serde.rs:333-368 | Roaring format parse + cardinality <= 150; trailing bytes ignored |
| `SizeOneVec` / `SenderSignedData` | base_types.rs:1867-1920 | len == 1 |
| `SenderSignedTransaction` | transaction.rs:3688-3714 | intent == (0,0,0) |
| `IntentScope/Version/AppId` | shared-crypto intent.rs | u8 in 0..=9 / {0} / 0..=2 |
| `GenericSignature` | signature.rs:299-316 | full inner parse, section 6 |
| `Signature` (nested only) | crypto.rs:743-759 | BYTES; flag 0/1/2 with length 97/98/98 |
| `MultiSig` / `MultiSigPublicKey` | multisig.rs | derived, but validated by `from_bytes` when inside GenericSignature |
| `MultiSigLegacy` / `MultiSigPublicKeyLegacy` | multisig_legacy.rs:251, 275-290 | base64 pk strings with curve validation; roaring bitmap |
| `PasskeyAuthenticator` | passkey_authenticator.rs:133-145 | JSON, challenge, P-256 pk & sig validity |
| `ZkLoginAuthenticator`/`ZkLoginInputs` | zk_login_authenticator.rs:255-268 | decimal strings, JWT header JSON, iss claim |
| `Bn254FqElement`/`Bn254FrElement` | fastcrypto-zkp zk_login_utils.rs | `^(0|[1-9][0-9]*)$` |
| `BytesRepresentation<N>` (CompressedSignature / PublicKey arms) | fastcrypto serde_helpers.rs | none (N raw bytes) |
| `ObjectPermissions` | object.rs:562, 637-647 | bit rules |
| `Party` | object.rs:817, 882-899 | sorted unique members; not CAO-equivalent |
| `Object` | object.rs:1100 | none (`from = ObjectInner`) |
| `MoveObject.contents`, `Event.contents`, `MovePackage.module_map` values | serde_as Bytes | none (same wire as SEQ U8) |
| `MovePackage.module_map`, `.linkage_table` | BTreeMap | bcs canonical key order |
| `AccumulatorValue::EventDigest` | nonempty | non-empty |
| `Duration` | serde | overflow check |
| `Envelope` | message_envelope.rs:42-56 | none |
| `EndOfEpochData`, `GasCostSummary`, `ObjectID`, `SuiAddress`, `SequenceNumber`(sui_serde), `Event.timestamp` etc. | `Readable<..>` adapters | human-readable only; binary path is the plain type |
| `AddressSeed` (zk_login_authenticator.rs:348) | decimal string <= 32 bytes | NOT on any YAML path (not used by ZkLoginInputs at the pinned rev) |
| `nonempty_as_vec` (transaction.rs:4303, 4386, 4478, 4488), `SuiKeyPair`, `HeaderMap`, `CoseSign1`, traffic_control | | not in the YAML type list |

Everything else in the YAML list is a plain `#[derive(Deserialize)]` whose only deviations are inherited
from the members above (checked: TransactionData(V1), GasData, ProgrammableTransaction, Command, Argument,
ChangeEpoch, GenesisTransaction/GenesisObject, AuthenticatorStateUpdate/Expire, ActiveJwk/JWK/JwkId (plain
Strings), RandomnessStateUpdate, RandomnessRound, ConsensusCommitPrologue V1-V4,
ConsensusDeterminedVersionAssignments, TransactionEvents, ExecutionDigests, SequenceNumber (no MAX check),
ProtocolVersion, TypeArgumentError, CommandArgumentError, PackageUpgradeError, DeleteKind,
ObjectInfoRequestKind, WriteAccumulatorStorageCost).

---------------------------------------------------------------------------------------------------

# (a) Derived-data accessors in `ST/transaction.rs`

Well-known constants (`ST/lib.rs:127-149`): object id = 32-byte address with the value in the low bytes.
`SUI_SYSTEM_STATE_OBJECT_ID = 0x5`, `SUI_CLOCK_OBJECT_ID = 0x6`, `SUI_AUTHENTICATOR_STATE_OBJECT_ID = 0x7`,
`SUI_RANDOMNESS_STATE_OBJECT_ID = 0x8`, `SUI_BRIDGE_OBJECT_ID = 0x9`, `SUI_ACCUMULATOR_ROOT_OBJECT_ID = 0xacc`
(also 0xa alias state, 0xc coin registry, 0xd display registry, 0x403 deny list - not used as inputs below).
`SUI_SYSTEM_STATE_OBJECT_SHARED_VERSION = SUI_CLOCK_OBJECT_SHARED_VERSION = OBJECT_START_VERSION = 1`
(`ST/object.rs:51`). `SharedInputObject::SUI_SYSTEM_OBJ = {0x5, 1, Mutable}` (:1828-1832).

`InputObjectKind` (:4572-4584, derives Ord in this variant order): `MovePackage(ObjectID)`,
`ImmOrOwnedMoveObject(ObjectRef)`, `SharedMoveObject { id, initial_shared_version, mutability }`.
`object_id()` (:4608) returns the id of any variant.

Coin-reservation digest test (`ST/coin_reservation.rs:41-66`):
`ParsedDigest::is_coin_reservation_digest(d)` <=> `d[12..32] == [0xac; 20]`. Parsed form: `d[0..8]` = u64 LE
reservation amount, `d[8..12]` = u32 LE epoch. `ParsedObjectRefWithdrawal::parse` additionally XORs the
object id with the 32 chain-identifier bytes (`mask_or_unmask_id`, :158-167).

### input_objects
- `CallArg::input_objects(&self) -> Vec<InputObjectKind>` (:779-805): Pure -> []; `ImmOrOwnedObject(ref)` ->
  [] if coin-reservation digest else `[ImmOrOwnedMoveObject(ref)]`; `SharedObject{..}` ->
  `[SharedMoveObject{same fields}]`; `Receiving` -> []; `FundsWithdrawal` -> [].
- `ProgrammableMoveCall::input_objects` (:1211-1224): `BTreeSet<ObjectID>` = {package} U packages of all
  type arguments, emitted in ascending ObjectID order as `MovePackage`.
  `add_type_input_packages` (:940-960): DFS over TypeInput; every `Struct` contributes `address` (as ObjectID)
  and recurses into `type_params`; `Vector` recurses; primitives nothing.
- `Command::input_objects` (:1414-1439): `Upgrade(_, deps, package_id, _)` -> deps in order then `package_id`
  (all `MovePackage`); `Publish(_, deps)` -> deps in order; `MoveCall` -> above;
  `MakeMoveVec(Some(t), _)` -> sorted packages of t; others -> [].
- `ProgrammableTransaction::input_objects(&self) -> UserInputResult<Vec<InputObjectKind>>` (:1577-1597):
  1. `input_arg_objects` = concat of `CallArg::input_objects` in input order.
  2. If any two of those share an `object_id()` -> `Err(UserInputError::DuplicateObjectRefInput)`.
  3. `command_input_objects: BTreeSet<InputObjectKind>` = union over all commands (dedups packages, sorted by
     derived Ord => ascending ObjectID since all are MovePackage).
  4. result = input args (input order) ++ package set (sorted). No dup check between the two groups.
- `EndOfEpochTransactionKind::input_objects` (:591-642): ChangeEpoch -> [{0x5,1,Mutable}];
  AuthenticatorStateExpire -> [{0x7, expire.authenticator_obj_initial_shared_version, Mutable}];
  BridgeCommitteeInit(v) -> [{0x9, v, Mutable}, {0x5,1,Mutable}]; StoreExecutionTimeObservations ->
  [{0x5,1,Mutable}]; WriteAccumulatorStorageCost -> [{0x5,1,Mutable}]; all the *Create variants and
  BridgeStateCreate -> [].
- `TransactionKind::input_objects(&self) -> UserInputResult<Vec<InputObjectKind>>` (:2016-2082):
  ChangeEpoch -> [{0x5,1,Mutable}]; Genesis -> []; ConsensusCommitPrologue V1-V4 -> [{0x6,1,Mutable}];
  AuthenticatorStateUpdate -> [{0x7, update.authenticator_obj_initial_shared_version, Mutable}];
  RandomnessStateUpdate -> [{0x8, update.randomness_obj_initial_shared_version, Mutable}];
  EndOfEpochTransaction(txns) -> flat_map in order, then **order-preserving dedup on the whole
  InputObjectKind value** (HashSet, first occurrence kept); ProgrammableTransaction /
  ProgrammableSystemTransaction -> `pt.input_objects()` returned directly. For the non-PT arms a final check:
  any repeated `object_id()` -> `Err(DuplicateObjectRefInput)` (e.g. two AuthenticatorStateExpire with
  different versions).
- `TransactionDataV1::input_objects` (:3008-3020): `kind.input_objects()?`, then if `!kind.is_system_tx()`
  append, in order, `ImmOrOwnedMoveObject(ref)` for every `gas_data.payment` entry whose digest is NOT a
  coin-reservation digest. No dup check between gas and the kind's inputs here.
  `fastpath_dependency_objects` (:3030-3049) splits that list into (owned refs, package ids) + receiving.

### shared_input_objects
- `ProgrammableTransaction::shared_input_objects(&self) -> impl Iterator<Item = SharedInputObject>` (:1690-1706):
  every `CallArg::Object(SharedObject{..})` in input order, **no dedup**.
- `EndOfEpochTransactionKind::shared_input_objects` (:644-683): same objects as its input_objects
  (ChangeEpoch/StoreExecutionTimeObservations/WriteAccumulatorStorageCost -> SUI_SYSTEM_OBJ;
  AuthenticatorStateExpire -> {0x7, v, Mutable}; BridgeCommitteeInit -> [{0x9, v, Mutable}, SUI_SYSTEM_OBJ]).
- `TransactionKind::shared_input_objects` (:1949-1987): ChangeEpoch -> SUI_SYSTEM_OBJ; prologues ->
  {0x6,1,Mutable}; AuthenticatorStateUpdate -> {0x7, v, Mutable}; RandomnessStateUpdate -> {0x8, v, Mutable};
  EndOfEpochTransaction -> flat_map in order, **NOT deduped** (unlike input_objects); PT / ProgrammableSystem
  -> pt's; Genesis -> empty.
- `TransactionDataV1::shared_input_objects(&self) -> Vec<SharedInputObject>` (:3022-3024) collects the above.
  `Envelope<SenderSignedData,S>::shared_input_objects` (:3961-3968) forwards.

### receiving_objects
- `CallArg::receiving_objects` (:807-817): only `Object(Receiving(ref))` -> [ref].
- `ProgrammableTransaction::receiving_objects` (:1599-1605): input order, no dedup.
- `TransactionKind::receiving_objects(&self) -> Vec<ObjectRef>` (:1996-2010): only `ProgrammableTransaction`;
  every other kind **including ProgrammableSystemTransaction** -> [].
- `TransactionDataV1::receiving_objects` (:3026-3028) forwards.

### move_calls
- `ProgrammableTransaction::move_calls(&self) -> Vec<(usize, &ObjectID, &str, &str)>` (:1708-1719): for each
  `Command::MoveCall` in order: (command index, package, module, function).
- `TransactionKind::move_calls` (:1989-1994): only `ProgrammableTransaction` (NOT ProgrammableSystemTransaction).
- `TransactionDataV1::move_calls` (:3004-3006) forwards.

### funds withdrawals / coin reservations
- `TransactionKind::get_funds_withdrawals(&self) -> impl Iterator<Item=&FundsWithdrawalArg>` (:2084-2097): only
  ProgrammableTransaction; `CallArg::FundsWithdrawal` inputs in order.
- `ProgrammableTransaction::coin_reservation_obj_refs` (:1679-1688): `ImmOrOwnedObject(ref)` inputs whose
  digest is a coin-reservation digest, in order. `TransactionKind::get_coin_reservation_obj_refs` (:2099-2104)
  only for ProgrammableTransaction.
- `TransactionDataV1::coin_reservation_obj_refs()` (private, :3612-3622): the kind's refs, then gas payment
  refs with a coin-reservation digest, in order. Trait version
  `coin_reservation_obj_refs(&self, chain_identifier) -> Vec<ParsedObjectRefWithdrawal>` (:3134-3141) maps
  `ParsedObjectRefWithdrawal::parse` (unmask id with chain id).
- `is_gas_paid_from_address_balance` (:2304-2313): `payment.is_empty() && kind is ProgrammableTransaction`.
  `is_gasless_transaction` (:2315-2317): that && `price == 0`.
- `get_funds_withdrawal_for_gas_payment` (:3596-3606): if paid from address balance and `budget > 0`:
  `MaxAmountU64(budget)` of `Balance<SUI>` from Sponsor if `sender != gas_owner` else from Sender.
- `has_funds_withdrawals` (:3117-3132): gas-from-balance with budget > 0, or any FundsWithdrawal input, or any
  coin reservation ref.
- `accumulate_funds_withdrawals` (:3542-3594) behind `process_funds_withdrawals_for_signing` (gas included) /
  `_for_estimation` (gas excluded) `-> UserInputResult<BTreeMap<AccumulatorObjId, (u64, TypeTag)>>`:
  list = explicit withdrawals (input order) ++ coin reservations resolved via `coin_resolver` ++ (optional)
  gas withdrawal. For each: amount 0 -> `InvalidWithdrawReservation`; owner = sender or gas_owner per
  `withdraw_from`; key = `AccumulatorValue::get_field_id(owner, &type_arg.to_type_tag())`
  (`to_type_tag` = `0x2::balance::Balance<T>`, :172-175); sum with `checked_add`, overflow -> error. Result is
  a BTreeMap (sorted by accumulator object id), first-seen type tag kept.
- `process_funds_withdrawals_for_execution(&self, chain_identifier) -> BTreeMap<AccumulatorObjId, u64>`
  (:3067-3115): explicit withdrawals ++ gas withdrawal summed per account id (asserts/unwraps instead of
  errors), then each coin reservation adds `reservation_amount` under
  `AccumulatorObjId::new_unchecked(parsed.unmasked_object_id)`.

### misc
- `required_signers(&self) -> NonEmpty<SuiAddress>` (:2968-2974): [sender] plus gas_owner if different.
- `gas()` = `&gas_data.payment`; `gas_owner()` = `gas_data.owner`.
- `is_system_tx` (:1858-1873): everything except `ProgrammableTransaction`. `is_end_of_epoch_tx` (:1875-1880):
  ChangeEpoch | EndOfEpochTransaction. `is_accumulator_barrier_settle_tx` (:1882-1888):
  ProgrammableSystemTransaction with a shared input {id 0xacc, Mutable}.
- `non_system_packages_to_be_published` (:1721-1725, :1441-1451): module byte vectors of Publish / Upgrade
  commands in order.
- `SenderSignedTransaction::get_signer_sig_mapping` (:3720-3734): BTreeMap address -> (sig index as u8, sig);
  needs address derivation from signatures (out of scope for a parser).

# (b) TransactionEffects accessors

`TransactionEffectsV2` (`ST/effects/effects_v2.rs:69-508`). All iterate `changed_objects` in stored order,
`lv = self.lamport_version`. Constants: `OBJECT_DIGEST_DELETED = [99; 32]`, `OBJECT_DIGEST_WRAPPED = [88; 32]`
(`ST/digests.rs:861-871`). `Owner::is_consensus()` = Shared | ConsensusAddressOwner | Party (`ST/object.rs:1024-1029`).

| method | filter -> output |
|---|---|
| `modified_at_versions() -> Vec<(ObjectID, SequenceNumber)>` (:82) | input `Exist(((v,_),_))` -> (id, v) |
| `old_object_metadata() -> Vec<(ObjectRef, Owner)>` (:110) | input `Exist(((v,d),o))` -> ((id,v,d), o) |
| `input_consensus_objects() -> Vec<InputConsensusObject>` (:123) | changed: input `Exist(((v,d),owner))` with `owner.is_consensus()` -> `Mutate((id,v,d))`; then chained `unchanged_consensus_objects` in order: ReadOnlyRoot((v,d)) -> `ReadOnly((id,v,d))`, MutateConsensusStreamEnded(s) -> same(id,s), ReadConsensusStreamEnded(s) -> same, Cancelled(s) -> `Cancelled(id,s)`, PerEpochConfig -> skipped |
| `created()` (:157) | (NotExist, ObjectWrite((d,o)), Created) -> ((id,lv,d), o); (NotExist, PackageWrite((v,d)), Created) -> ((id,v,d), Immutable) |
| `mutated()` (:182) | (Exist, ObjectWrite((d,o))) any id_op -> ((id,lv,d), o); (Exist, PackageWrite((v,d))) -> ((id,v,d), Immutable) |
| `unwrapped()` (:199) | (NotExist, ObjectWrite((d,o)), None) -> ((id,lv,d), o) |
| `deleted()` (:219) | (Exist, NotExist, Deleted) -> (id, lv, DELETED) |
| `unwrapped_then_deleted()` (:239) | (NotExist, NotExist, Deleted) -> (id, lv, DELETED) |
| `wrapped()` (:259) | (Exist, NotExist, None) -> (id, lv, WRAPPED) |
| `written()` (:279) | by (output, id_op): (NotExist, Deleted) -> (id,lv,DELETED); (NotExist, None) -> (id,lv,WRAPPED); (ObjectWrite((d,_)), _) -> (id,lv,d); (PackageWrite((v,d)), _) -> (id,v,d); AccumulatorWriteV1 and (NotExist, Created) -> skipped |
| `transferred_from_consensus()` (:303) | (Exist((_, ConsensusAddressOwner)), ObjectWrite((d, AddressOwner \| ObjectOwner \| Immutable)), None) -> (id,lv,d) |
| `transferred_to_consensus()` (:326) | (Exist((_, AddressOwner \| ObjectOwner)), ObjectWrite((d, ConsensusAddressOwner)), None) -> (id,lv,d) |
| `consensus_owner_changed()` (:349) | both ConsensusAddressOwner, id_op None, `old_owner != new_owner` -> (id,lv,d) |
| `object_changes() -> Vec<ObjectChange>` (:381) | every entry except output AccumulatorWriteV1; input (v,d) from Exist; output (lv,d) for ObjectWrite, (v,d) for PackageWrite, None for NotExist; carries id_operation |
| `published_packages()` (:414) | output PackageWrite -> id |
| `accumulator_events()` (:427) / `accumulator_updates()` (:484) | output AccumulatorWriteV1(w) -> (id, w) |
| `gas_object() -> Option<(ObjectRef, Owner)>` (:440) | `gas_object_index.map(i => changed_objects[i])` (**panics if out of range**); ObjectWrite((d,o)) -> ((id,lv,d), o); NotExist -> ((id,lv,DELETED), AddressOwner(SuiAddress::ZERO)); anything else panics |
| `unchanged_consensus_objects()` (:480) | clone |
| `lamport_version()` (:106) | field |

Note the (NotExist, ObjectWrite, Deleted), (Exist, NotExist, Created), (NotExist, NotExist, None/Created)
combinations fall into no bucket (except `mutated` ignores id_op for Exist+write).

Provided methods on the enum (`ST/effects/mod.rs`):
- `all_changed_objects()` (:207-225): mutated (WriteKind::Mutate) ++ created (Create) ++ unwrapped (Unwrap).
- `all_removed_objects()` (:227-238): deleted (Delete) ++ wrapped (Wrap).
- `all_tombstones()` (:241-248): deleted ++ unwrapped_then_deleted ++ wrapped, mapped to (id, version).
- `mutated_excluding_gas()` (:251-257): mutated filtered by id != gas_object id.
- `stream_ended_mutably_accessed_consensus_objects()` (:366-378): ids of `MutateConsensusStreamEnded`.

`TransactionEffectsV1` (`ST/effects/effects_v1.rs:130-350`): created/mutated/unwrapped/deleted/
unwrapped_then_deleted/wrapped = clones of the stored vectors; `gas_object()` = `Some(stored)`.
`modified_at_versions()` (:143-157) = stored list minus ids present in `unwrapped_then_deleted`.
`lamport_version()` (:170-172) = `1 + max(version in stored modified_at_versions)` (0 if empty -> 1; asserts
max != u64::MAX). `input_consensus_objects()` (:178-190): each `shared_objects` ref -> `Mutate` if its id is in
(unfiltered) `modified_at_versions`, else `ReadOnly`. `unchanged_consensus_objects()` (:332-344): the ReadOnly
ones as `ReadOnlyRoot((v,d))`. `transferred_*`/`consensus_owner_changed`/`accumulator_*` -> []; `object_changes`
(:231+) is synthesized from the six lists; `old_object_metadata`, `published_packages`, `written` are
`unimplemented!()`.
