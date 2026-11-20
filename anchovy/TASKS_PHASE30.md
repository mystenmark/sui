# Phase 30: following sui main

Anchovy is rebased onto sui main of 2026-11-19 (two weeks of upstream), and
the PRD now asks for a rebase at least once a week.

## Done
- Sui cut execution version 5, which `latest` now holds and protocol 143
  enables. Protocols up to 142 run on the frozen v4 copy, which is what
  anchovy's executor ports, so the reference crates it builds on are now
  v4's (`sui-adapter-v4`, `sui-verifier-v4`, `move-vm-runtime-v4`,
  `move-bytecode-verifier-v4`). The natives' nitro call matches v4's.
- fastcrypto at sui's revision; fastcrypto-tbls is fastcrypto-threshold.
- Sui renumbered protocol 141 (#28590): it now holds only the complete
  effects size estimate and the 1024 accumulator event limit, both ported.
  The old 141 is 142, which also enables defer_owned_object_double_spend
  everywhere (anchovy has run it since phase 16). Anchovy runs up to 142.
- Wire format: `WithdrawFrom::FrozenAddress`, `FreezeListCreate` and
  `ExecutionErrorKind::TransactionExpired`. Validation rejects the first two
  with the reference's errors; v4 execution treats them as unreachable.
- API changes: `SuiGasStatus::for_execution` and `check_gas_price`, the
  expiration argument of early errors, consensus-core's `ConsumerMonitor`.
- Tests that execute run at `MAX_PROTOCOL_VERSION`, not sui's highest.
- The oracle follows the new APIs and writes vectors only up to anchovy's
  highest version (a validator test checks they agree). The regenerated
  validity, mutation, input and consensus vectors match at every version
  anchovy runs.

## Found
- Validation differs from the reference only at protocol 144, whose
  timestamp expiration anchovy does not implement; hence the oracle's cap.
- `aggregator::aggregate a_committee_of_a_hundred_certifies_at_two_thirds`
  was reported leaky once by nextest (a crate this phase does not touch).
- At the renumbered 141 the compat tests' packages fail to publish
  (`VMVerificationOrDeserializationError`): they use framework features that
  arrive in 142. They pass at 142.

## Remaining
- Execution v5 (protocol 143): a port of the new `latest` adapter, natives
  and verifier, plus the Move VM changes behind it.
- Timestamp expiration (protocol 144, devnet): validity checks and the
  early error. Execution asserts it is off.
- The validator-side consensus transaction pool sui now uses by default:
  gas-price ordering and shedding, one pending transaction per gas version
  with `OwnedObjectConflict` (#28519), pull-time owned-version checks
  (#28520), byte bounds and a time to live. Anchovy submits straight into
  consensus-core's channel.
- The effects size estimate is compared with the reference without
  accumulator events; the compat workloads cover them.
- The mainnet validity corpus and signature vectors were not regenerated
  (they need checkpoint files).
- Derp sweeps, with derp's latest main.
