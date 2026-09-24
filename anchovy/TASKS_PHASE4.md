# Phase 4 progress: static transaction validation

Plan: `IMPLEMENTATION_PLAN_PHASE4.md`. Branch `mlogan-phase4`, to be merged
into `anchovy-main`.

## Status: steps 0 to 5 done

## Done

0. `crates/protocol-config`: `sui-protocol-config` copied, then:
   - dropped: Antithesis reachability, `clap`, `schemars`, env-var
     overrides, RPC `render`, Move verifier/binary config builders, seeded
     test overrides;
   - `shims.rs`: `AccountAddress` (same serde form), `Hex`, `Base58`,
     `VARIANT_COUNT_MAX`, `in_integration_test` (false);
   - `macros.rs`: `protocol_config!` and `feature_flags!` (`macro_rules!`,
     one field per recursion step) replace the three derives. Scalar fields
     get the panicking getter and by-name lookup; bool flags get forwarding
     getters on `ProtocolConfig`. Fields are `pub`, standing in for
     `_as_option` and the generated test setters.

   Dependencies: `serde`, `serde_json`, `tracing` (no default features),
   `bs58`. The reference's 414 snapshots (every version on Unknown, Mainnet
   and Testnet), renamed for the crate, pass unchanged.

1. `crates/validation`: `Context` (the reference's
   `TxValidityCheckContext`), `Error` carrying the reference's error
   variant name, and `tests/reference.rs`, which replays
   `tests/data/validity.vectors` (written by `sui-oracle
   --validity-vectors`) against our checks. Each vector is a transaction
   plus a context; the oracle runs the reference under every protocol
   version on Unknown, Mainnet and Testnet and records the verdict, with
   runs of equal verdicts compressed into version ranges. 300,564 cases.
2. `TransactionData::validity_check`, in the reference's order:
   expiration and allowed proposers; funds withdrawals and coin
   reservations; address-balance gas; gas object count; coin reservations
   as gas (the SUI balance id derived as the reference's dynamic field id,
   in `accumulator.rs`); price cap and budget bounds; `kind.rs` (system
   kind flags, PTB command/input/package/receiving limits, duplicate
   inputs, pure size, accumulator type nodes, publish limits, per-command
   checks, type arguments in the reference's stack order, identifiers,
   argument indices, randomness); `gasless.rs`; sponsorship. Plus
   `check_gas_price`, the static part of `SuiGasStatus::new`.
   `validity_check` takes a `&Bump` for its temporaries.

   Crafted vectors come from limits read off every version's config
   (just under, at, over each distinct value), so they follow the table.
   Planted bugs (an off-by-one; reversed type-argument order) are caught.

3. and 4. (done together: the scheme gate needs parsed signatures)
   `signature.rs` parses `GenericSignature` as the reference's
   `from_bytes`: exact lengths for single-key schemes; the new multisig
   (the `messages` view, built into the caller's `Bump` through the new
   `messages::arena::BumpAlloc`) with `init_and_validate`, falling back to
   legacy multisig (roaring bitmap via the `roaring` crate, keys strictly
   base64-decoded into a stack buffer and point-checked by fastcrypto);
   passkey (client data with `passkey-types`' serde attributes mirrored,
   base64url challenge, Secp256r1 key and signature by fastcrypto);
   zkLogin (fastcrypto-zkp's `ZkLoginInputs` through `bcs`, then
   `init`). Each also yields the length the reference re-serializes it to,
   which differs from the wire for legacy multisig (canonical bitmap) and
   zkLogin (field elements reduced mod p).
   `sender_signed.rs`: the deserialization checks our parser defers
   (intent `00 00 00`, non-empty allowed proposers, identifiers in
   withdrawal types, signatures), then `SenderSignedData::validity_check`:
   scheme gating, no system transactions, size limits on the reference's
   re-serialized size, `TransactionData::validity_check`. Returns the
   parsed signatures and that size.
   Vectors: `signatures.vectors` (`sui-oracle --signature-vectors`, 150
   signatures incl. sui's zkLogin test vectors, each with the reference's
   variant and re-serialized length) and `sender_signed` cases in
   `validity.vectors` (verdict `ok:<size>` compares sizes too).

5. `verify.rs`: `Verifier` (the epoch's JWKs and the protocol's
   verification settings, as sui's `SignatureVerifier`) and
   `verify_signatures`: one signature per required signer, signer map by
   address (later wins, walked in address order), required signers or
   caller-supplied aliases present, then each scheme's check: single keys
   (address over fastcrypto's re-encoded key, then the signature), multisig
   (both formats; members paired with the bitmap's bits and truncated as
   the reference does in release; nested zkLogin and passkey), zkLogin
   (epoch bounds, unpadded or legacy padded address, provider, ephemeral
   signature, `verify_zk_login` with no proof cache), passkey (address,
   challenge = transaction digest, Secp256r1 over authenticator data and
   the client data's SHA-256).
   Vectors: `verify` cases in `validity.vectors` (transactions signed with
   every scheme, then broken; zkLogin from sui's test proof and JWK, which
   the file carries on its `jwks` line), at epochs 4, 10 and 11.
   Dependencies build with `opt-level = 2` in dev and test: arkworks is
   otherwise too slow for the proof checks.

Corpus: `sui-oracle --validity-corpus` records the reference's
`validity_check` and verification verdicts for every transaction of a
mainnet checkpoint under a fixed context (the checkpoint's epoch,
mainnet's chain id and latest config, RGP 1, no JWKs).
`tests/corpus.rs` compares ours: the checked-in checkpoint always, and
the fetched corpus when present (2,332 transactions in 64 checkpoints:
1,683 pass everything, 660 system transactions, 2 `InvalidArgumentIndex`
under the latest config, 1 zkLogin without its JWK; all agree).

Mutation testing (after review: the corpus is valid transactions and the
crafted vectors mostly break one check at a time):
`sui-oracle --mutation-vectors` takes valid seeds (the crafted shapes and
the checked-in mainnet checkpoint's transactions, re-sent from our keys),
applies one to three random field-aware changes (gas, sender, expiration,
inputs, commands, argument indices, type arguments, identifiers, publish
counts, randomness, system kinds) and sometimes breaks the signatures,
then records the reference's verdict on everything static a validator
checks on submission (`full`: decoding, `validity_check`, signature
verification) over every tenth and the last nine protocol versions on
each chain. 3,000 transactions, seeded and reproducible, gzipped
(`mutations.vectors.gz`). `validation::check` is that same sequence on our
side. All agree; neither side panics. `tests/coverage.rs` fails if an
`ErrorKind` has no vector, except `IncorrectSigner` and `InvalidAddress`,
which cannot be reached end to end (listed with reasons). The mutator is
meant to grow with the stateful layer, whose rejections it will check the
same way.

## TODO

- Optimize signature verification (see the TODO in `verify.rs`): heap use
  in fastcrypto's Secp256r1 verification (and so passkey), zkLogin
  re-parsing and no proof cache, serde_json for passkey client data.
  Accepting other transactions allocates nothing (`tests/allocations.rs`).

## Deferred to the stateful layer

Checks on the reference's signing path that need object state, so are
not in `validation`; each must land with object state:

- `check_replay_protection` (sui-transaction-checks, run at signing for
  every version): without a one- or two-epoch window and without gas
  objects, a transaction needs an address-owned or coin-reservation input
  (`InvalidExpiration`). Telling address-owned from immutable inputs needs
  the objects. With `relax_valid_during_for_owned_inputs` this is the only
  replay check for address-balance gas.

An audit of the rest of the signing path (`handle_submit_transaction`,
`handle_vote_transaction`, sui-transaction-checks, the consensus vote)
found more checks outside the functions we mirrored. Static, or always
rejecting whatever the state (only the error depends on it):

- `ObjectInputArityViolation` (`check_objects`): no input objects at all,
  when coin reservations are off and gas comes from the address balance.
- `GasBalanceTooLow` (`check_gas_data`): gas paid only with coin
  reservations whose amounts sum below the budget.
- A gas object repeated in `payment`, or also a PTB input: always
  rejected (`MutableObjectUsedMoreThanOnce` or a state-dependent error);
  the gas refs are not in validity_check's duplicate check.
- A receiving object that is also an input or repeated
  (`DuplicateObjectRefInput`, unless already received at that version);
  receiving or owned versions at or above `SequenceNumber::MAX`.
- `SenderAllowance` whose allowance object is not among the inputs, and
  withdrawal sums per account overflowing u64 (`InvalidWithdrawReservation`).
- Shared system objects: Clock, Random and the accumulator root only
  immutable; other system ids only the listed ones
  (`ImmutableParameterExpectedError`, after loading).
- Gasless transactions with shared system-object inputs (`Unsupported`).
- Package metering and the `<SELF>` identifier at publish
  (`PackageVerificationTimeout`; limits from node config).

Request- and node-level, for the Phase 5 handler: submit type, ping and
empty requests, batch size, `RepeatedTransactions`, soft-bundle
`GasPriceMismatchError`, `TotalTransactionSizeTooLargeInBatch`,
`check_self_allowed_proposer` (`ProposerNotAllowed`), and on the
consensus side the alias-claim and immutable-object-claim checks.

Stateful: deny lists and node deny config, object existence, versions,
digests and ownership, locks, balances and allowances, coin deny lists,
aliases (which also make `SignerSignatureAbsent` stateful), overload and
in-flight state, executed-transaction lookups.

## Findings

- The reference panics in two places after its static checks pass,
  recorded as `panic` verdicts: `SuiGasStatus::new_with_budget` asserts a
  non-zero gas price (a zero reference gas price lets price 0 through),
  and overflows on a huge price under gas models without a price cap.
- `SuiCostTable::new` multiplies `base_tx_cost_fixed * price` under
  `#[with_checked_arithmetic]`, so an overflow panics in every build. It
  cannot happen: the multiplier exists only from v18, where the price cap
  is checked first. Ours saturates.
- `get_gasless_allowed_token_types` caches by protocol version only, not
  chain: a process that validates for several chains can get another
  chain's list. The oracle's loop order changes version on every call, so
  it never hits a stale entry.
- An empty `AllowedProposers.proposers` is rejected by the reference at
  deserialization; our parser accepts it, so step 4 must check it.
- The reference accepts, when parsing: a legacy bitmap with trailing
  bytes and an empty legacy bitmap; a passkey key with the SEC1 compact
  tag `0x05`; high-s passkey signatures; zkLogin field elements of any
  length at or above the modulus (reduced); JSON with duplicate unknown
  keys (last wins).
- fastcrypto-zkp's `decode_base64_url` adds a claim's length in `u8`: a
  256-character claim panics with overflow checks on (debug and tests)
  and wraps harmlessly in release. Both workspaces build fastcrypto-zkp
  without overflow checks, as release does.
- Legacy multisig and zkLogin parsing allocate on the heap (roaring,
  serde); both are rare and zkLogin's proof check dominates its cost.
- System kinds carry deserialization rules we do not check (genesis
  objects, durations); a user-submitted one is rejected as a system
  transaction, where the reference reports a deserialization error.
- The reference pairs multisig signatures with the bitmap's bits via
  `zip_debug_eq`: in release it stops at the shorter, so signatures past
  the bits are never checked (garbage is accepted) and extra bits are
  ignored; in debug it panics (`debug_fatal`), so a user transaction can
  crash a debug-built validator. We truncate, as release does. The oracle
  builds mysten-common without debug assertions to see release behavior.
- An Ed25519 signature whose key fastcrypto accepts but that is not the
  signer's derives a different address: the reference reports a missing
  signer, not a bad signature.
- The vector file is 1.3 MB, mostly limit-boundary transactions (2,049
  package dependencies, 1,025 commands, 16 KB pure arguments).

## Reference survey (checklist)

Where the reference does static checks on a submitted transaction
(`crates/sui-core/src/authority_server.rs` `handle_submit_transaction`):

- `SenderSignedData::validity_check` (sui-types `transaction.rs`): scheme
  gating by flags (`upgraded_multisig_supported`, `zklogin_auth`,
  `passkey_auth`); no system transactions; re-serialized size ≤
  `max_tx_size_bytes` (and `gasless_max_tx_size_bytes`); then
  `TransactionData::validity_check`.
- `TransactionData::validity_check(TxValidityCheckContext)`, in order:
  expiration / `ValidDuring` / allowed proposers; funds withdrawals and coin
  reservations; address-balance gas; gas object count; coin reservations as
  gas; gas price cap and budget bounds (`SuiCostTable` from gas_v2);
  `TransactionKind::validity_check` (PTB: command count, input objects and
  duplicates, per-input checks, publish count, per-command checks incl.
  type-argument count/depth and identifiers, argument indices, randomness
  restrictions; system kinds: flags only); gasless rules; sponsorship.
- Signing-time static gas check: `SuiGasStatus::new` (price ≥ RGP, price <
  `max_gas_price`).
- Signature verification (`verify_sender_signed_data_message_signatures`,
  `GenericSignature::verify_authenticator`): signature count equals required
  signers; signer mapping; Ed25519 / Secp256k1 / Secp256r1; MultiSig (and
  legacy); zkLogin (epoch bounds, JWKs, providers); Passkey.
- Deserialization-time checks deferred by our parser: see
  `docs/REFERENCE_STRICTNESS.md` §5–6 (`GenericSignature` contents,
  `Identifier` in `TypeTag`).

Reference unit tests to mirror as crafted vectors: sui-types
`unit_tests/{messages_tests, allowed_proposers_tests,
address_balance_gas_tests, balance_withdraw_tests, multisig_tests,
zk_login_authenticator_test, passkey_authenticator_test}.rs`; sui-core
`unit_tests/{transaction_tests, submit_transaction_tests, gas_tests,
gas_data_tests, move_package_publish_tests, transfer_to_object_tests}.rs`.

## Remaining

5. Signature verification, with signed vectors.
