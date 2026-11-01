# Phase 20: simplification audit

An audit after phase 19, to remove complexity left by trial and error, and
check that the waiter pattern is used where it should be. The candidates
below were checked against the code. Nothing has been changed yet.

## Waiter pattern

- [ ] **The randomness gate checks a table the synced scheduler owns.**
  `RandomnessGate::release` reads `ChunkLock::held`, and
  `GateInput::Released` is a wake-up signal with no data. Proposal: chunks
  pass through a waiter on `ChunkFree(height)` (available when the lock
  doesn't hold the chunk; announced by the synced scheduler on release)
  and `RoundBytes(round)` (announced by the signature collector). The gate
  then only puts chunks back in height order and inserts the update.
  `Released`, the gate's `chunk_lock`, and its private bytes table go away.
- [ ] **The aggregator's fork-check wait is a no-op.** It certifies the
  local summary, so `BuiltWaitItem::Certified`'s `check_fork` can never
  fail. Merge it with `Synced`: one `Certified` item, fork-checked and then
  sent to the committer, so the aggregator stops sending to the committer
  directly.
- [ ] Borderline, keep unless asked:
  - backpressure's parked oneshots: an either/or condition that mirrors
    the reference;
  - the epoch closer's `closing` handshake with the state hasher;
  - p2p polling the gate for a stuck round;
  - `submit_blocking` on consensus's lane.

## Dead or leftover

- [ ] The perpetual `state_sync` queue: production drains it and throws it
  away ("No state sync yet"). Tests use it to see what anchovy certified.
  They need another probe.
- [ ] The p2p stuck-round fetcher (`fetch_missing`, `GateInput::Waiting`)
  overlaps the synced scheduler's verified round feeding. Uncertain: it's
  the only backstop for a round outside the collector's window. Remove only
  if the compat restart and lagging tests pass without it.
- [ ] `Executable.resolved_base` and `Executable.height` are never read
  outside tests.
- [ ] `checks::InputsChecked`, `check_inputs` and `InputsCheckedTransaction`
  have had no callers since `InputChecker` was removed.
- [ ] `Store::clear_uncommitted` and `write_through` are test-only, and so
  are several `HandledCommit` stats fields.
- [ ] Stale comments and docs: "congestion control is not here yet",
  "cancelled transactions do not exist yet", "No state sync yet", and
  `consensus_pipeline.md`'s "Not covered yet" and `InputChecker`
  references.

## Duplication

- [ ] `Chunk` and `ChunkSettlement` repeat five fields, and a `digests`
  vector runs in parallel with `entries` from `decide` to the gate. The gate
  could build the settlement from the chunk.
- [ ] Synced contents are verified again (the digest recomputed) in
  `split`, `executed` and, per ended-stream input, `deleter`. Parse
  verified bytes without the check.
- [ ] The change-epoch case in `synced::split` repeats the chunk loop.
- [ ] Settlement's `Pending.base` copies `chunk.root.base`. The
  `SharedAccess` → `InputKey` mapping is written in two crates.
- [ ] The beacon takes two paths: the group key goes separately from
  `BeaconEvent`, restore and per-commit DKG handling are separate, and
  `handle_dkg` decodes bytes `Dkg::handle` just encoded.
- [ ] The commit handler builds its estimator twice (`new`, then
  `with_stored_observations`). The last commit index is tracked in three
  places.
- [ ] The `push` helper is copied six times; it should become one in
  `workqueue`.
- [ ] Builder `Option`s that production always sets (`chunk_lock`,
  `backpressure`, `fork_checks`, `epoch_closer`) exist only so tests can
  leave them out.
- [ ] Hand-written tuple codecs (`CommitRecord`, `BuiltCheckpoint`,
  generation values) could derive serde, with the same BCS bytes.
- [ ] `ObjectId::from_u16(0xacc)` is defined five times.

## Rule violations found on the way

- [ ] `synced::contents::chunk_ends` and `item::executable` build std
  `Vec` temporaries; per the PRD these should come from the arena.
