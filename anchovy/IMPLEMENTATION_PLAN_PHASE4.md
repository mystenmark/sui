# Phase 4: static transaction validation

Requirements are in `PRD.md`, Phase 4: implement and test transaction
validity checking, meaning the static checks. Phase 5 moves this into a
processor on the work-queue architecture; Phase 4 is the checks themselves,
as a library.

## Scope: what "static" means

A check is static if it depends only on the transaction's bytes and on
epoch-level inputs: protocol config, epoch, chain identifier, reference gas
price, committee size, and (for zkLogin) the epoch's JWKs. Anything that
reads objects, locks, deny lists, the store or overload state is stateful
and out of scope.

The reference applies the static checks in these places, and all of them
are in scope:

1. **Deferred deserialization checks.** Per the PRD clarification, our
   parser only builds the in-memory form; the rest of what sui-types'
   `Deserialize` impls reject is checked here. For a submitted transaction
   that means the contents of each `GenericSignature` (scheme flag, lengths,
   MultiSig structure, legacy MultiSig's roaring bitmap and base64 keys,
   zkLogin inputs, passkey client data) and `Identifier` grammar inside
   `TypeTag`s. See `docs/REFERENCE_STRICTNESS.md`.
2. **`SenderSignedData::validity_check`**: signature schemes the protocol
   allows, no system transactions, size limits, then
   **`TransactionData::validity_check`**: expiration and `ValidDuring`,
   allowed proposers, funds withdrawals and coin reservations,
   address-balance gas, gas object count, gas price and budget, per-kind
   checks (PTB command, argument, input, type-argument and identifier
   limits; argument indices; randomness restrictions; publish limits),
   gasless rules, sponsorship.
3. **The static part of signing-time gas checks**: gas price at least the
   reference gas price (`SuiGasStatus::new`), which is otherwise only
   checked after object loads.
4. **Signature verification**: signature count against required signers,
   signer-to-signature mapping, and verification for Ed25519, Secp256k1,
   Secp256r1, MultiSig (both formats), Passkey and zkLogin.

Out of scope: stateful checks (overload, locks, deny lists, executed or
in-flight transactions, replay protection by owned inputs, address
aliases); request-level checks (ping shape, batch sizes, repeated digests,
soft-bundle gas prices), which belong to the handler and land with Phase 5;
wiring validation into the RPC path (Phase 5).

## Design

### Crate

`crates/validation`, depending on `messages`. Functions take parsed views
(`&SenderSignedData<'_>`, `&TransactionData<'_>`) and a `Context`, and read
top to bottom in the reference's order, one function per reference
function, so each can be compared against its counterpart.

### Protocol config

`crates/protocol-config` is a copy of `sui-protocol-config` (done first
on this branch). Its dependencies are cut to `serde`, `serde_json`,
`tracing` and `bs58`. Small stand-ins in `shims.rs` replace the Move,
fastcrypto and mysten-common imports, and two `macro_rules!` in
`macros.rs` replace the derive macros. The copy keeps the reference's
field names, getters and version table, so the checks read
`ProtocolConfig` exactly as the reference does. The reference's snapshot
test (every version, three chains) passes unchanged, which shows the copy
yields the same values.

`Context` is the reference's `TxValidityCheckContext` (`&ProtocolConfig`,
epoch, chain identifier, reference gas price, committee size) plus what
signature verification needs (zkLogin environment, JWKs, supported
providers). Oracle vectors record a protocol version and chain, and tests
build the config with `ProtocolConfig::get_for_version`.

### Errors

One enum whose variants carry the reference error variant names
(`SizeLimitExceeded`, `TransactionExpired`, `Unsupported`, …) and the
detail needed to act on them. Tests compare variant names with the
reference, not messages.

### Crypto

`fastcrypto` for every primitive (PRD exception): Ed25519, Secp256k1,
Secp256r1, Blake2b, and `fastcrypto-zkp` for zkLogin. Passkey client data
is JSON, parsed with `serde_json`.

## Testing: differential against the reference

The reference is the oracle. `tools/sui-oracle` gains modes that emit
vectors, each a context, transaction bytes, and the reference's verdict
(`ok` or the error variant):

- **Crafted cases**: transactions built with sui-types' builders and then
  broken one field at a time to hit each check and each boundary (limit,
  limit − 1, limit + 1), under the current protocol version and the
  versions where a gating flag flips.
- **Signed cases**: keys of every scheme sign real transaction data;
  variants corrupt a signature, swap signers, drop or add a signature,
  and cross the multisig threshold. zkLogin and passkey use sui's own
  test vectors (`zklogin_test_vectors.json`, the passkey unit tests).
- **Mainnet corpus**: every transaction in the corpus, with the
  reference's verdict under a fixed context, for broad coverage of the
  passing path.

Our result must match the reference's verdict on every vector. The
reference's unit tests (listed in the survey in `TASKS_PHASE4.md`) are the
checklist for what the crafted cases must cover.

## Steps

0. `crates/protocol-config`: the reference's protocol config, dependencies
   stripped, derive macros replaced with `macro_rules!` (done).
1. `validation` crate: `Context`, `Error`; the vector format and a test
   runner that compares verdicts.
2. `TransactionData::validity_check`, in the reference's order, with
   crafted vectors per check group: expiration and allowed proposers; gas
   count, price and budget; transaction kind and PTB checks; funds
   withdrawals, coin reservations and address-balance gas; gasless rules;
   sponsorship. Plus the reference-gas-price floor.
3. `SenderSignedData::validity_check`: scheme gating, system transaction
   rejection, size limits. Corpus vectors.
4. Deferred signature parsing: `GenericSignature` into a parsed form, with
   the reference's structural checks, and `Identifier` grammar in type
   tags.
5. Signature verification: signer mapping and count, simple schemes,
   MultiSig, Passkey, zkLogin. Signed vectors.
