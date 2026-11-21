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
- Address aliases: signature checks read each required signer's live
  `AddressAliases` object (its id derived without allocating); any listed
  alias may sign, and the object's version is what a submission claims and
  what a vote compares the claim with. The signature cache's key holds the
  versions. Verification of a signer the store has looked up before
  allocates nothing; a signer's first lookup fills the store's cache.
- The complete effects size estimate is compared with the reference with
  accumulator events (wide type tags, repeated objects, event digests).
- The oracle follows the new APIs and writes vectors only up to anchovy's
  highest version (a validator test checks they agree). The regenerated
  validity, mutation, input and consensus vectors match at every version
  anchovy runs.

## Found
- Validation differs from the reference only at protocol 144, whose
  timestamp expiration anchovy does not implement; hence the oracle's cap.
- At the renumbered 141 the compat tests' packages fail to publish
  (`VMVerificationOrDeserializationError`): they use framework features that
  arrive in 142. They pass at 142.

- Derp sweep (derp main 333e342), seeds 1-24 of all 34 compat tests: three
  composite runs fail (full composite seed 9, full through stalls seed 4,
  composite through stalls seed 18), all the same way. Anchovy claims alias
  version `None` for every signer (it does not read `AddressAliases`
  objects), so once the workload enables a sender's aliases, sui's voters
  reject that sender's transactions anchovy submits with `AliasesChanged`;
  the client gives up after 15-19 attempts. Anchovy's voter likewise
  rejects sui's `Some` claims. Every other run passed. Fixed (address
  aliases, above); seeds 1-48 of the four composite workloads then pass.
- A derp seed replays identically only with the same `--scratch` path: the
  host directory is in the guest's environment.
- nextest reported a leak once each in two tests this phase does not touch
  (`aggregator::aggregate`, `validator::consensus
  an_executed_transaction_is_accepted_though_its_inputs_are_spent`).

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
- The mainnet validity corpus and signature vectors were not regenerated
  (they need checkpoint files).
- The consensus transaction pool and execution v5 are not planned yet.
