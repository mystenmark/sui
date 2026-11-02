# Phase 22: the composite workload through the validators, across epochs

Phase 21's remaining item: the composite workload submitted to the
validators directly, anchovy among them, and epoch changes under load.

## Plan

- [x] Submit through `LocalValidatorAggregatorProxy`: sui's transaction
  driver picks validators, so anchovy's RPC takes a share of submissions and
  soft bundles. The full node still serves reads, the system state and
  reconfiguration. The failure mix matches the full-node runs.
- [x] Epoch changes under load: 61 s epochs, a 2-minute workload, at least
  two epochs ended.
  - The workload stamps address-balance gas and coin reservations with the
    epoch its `SystemStateObserver` last read, every 60 s, valid for that
    epoch and the next: shorter epochs expire them.
  - The harness moves anchovy into the next epoch only as it is polled; the
    workload now runs as a task while the test thread polls. Blocked, anchovy
    started each epoch ~80 s late, missed its DKG (the sui pair's 350 of 525
    falls short of the 351 confirmations need), so randomness stopped and
    deferred randomness transactions held sui's epoch close to its 120 s
    deadline.
- [ ] Then: catching up with sui `main` (next phase).

## Known gaps found

- Once, in a run where the harness stalled anchovy ~80 s at an epoch
  boundary, anchovy's executor panicked on catching up: "a certified coin
  reservation names a field" (`execution/src/native.rs`). Not reproduced in
  ten more stalled runs. Ruled out: the synced path's funds status (it
  carries the quorum's insufficient-funds outcome), its root versions (only
  the barrier advances the root), and bounded reads of fields deleted since.
- Sui's writeback-cache coherence check (debug builds) panics under this
  workload; a false positive of the check itself, being fixed separately.
