# Phase 4 progress: static transaction validation

Plan: `IMPLEMENTATION_PLAN_PHASE4.md`. Branch `mlogan-phase4`, to be merged
into `anchovy-main`.

## Status: step 0 done, step 1 next

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

1. `validation` crate: `Context`, `Error`, vector format, runner.
2. `TransactionData::validity_check` and the RGP floor, with crafted vectors.
3. `SenderSignedData::validity_check`, with corpus vectors.
4. Deferred signature parsing and `Identifier` grammar.
5. Signature verification, with signed vectors.
