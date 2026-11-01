# Phase 20: simplification audit

An audit after phase 19, to remove complexity left by trial and error and
to check that the waiter pattern is used where it should be. The
candidates were checked against the code before being changed.

## Waiter pattern

- [x] **The gate waiter.** Chunks wait on `GateKey::ChunkFree(height)`,
  available once the chunk lock doesn't hold the chunk and resolved by the
  synced scheduler after it releases it. A chunk headed by a randomness
  update also waits on `RoundBytes(round)`, available once the epoch store
  records the bytes and resolved by the signature collector. The gate now
  only puts chunks back in height order and inserts the update. Removed:
  `GateInput` (`Released`, `Bytes`, `Waiting`), the gate's chunk-lock check
  and bytes table, and the p2p stuck-round fetcher, which the synced
  scheduler's verified round feeding covers.
- [x] **The aggregator's fork check is removed.** It was a no-op: the
  certificate is made over the local summary. Synced checkpoints keep
  theirs.
- Borderline, kept:
  - backpressure's parked oneshots: an either/or condition that mirrors the
    reference;
  - the epoch closer's `closing` handshake with the state hasher;
  - `submit_blocking` on consensus's lane.

## Dead or leftover

- [x] `Executable.resolved_base` and `height`: never read outside tests. The
  resolver bench recorded generations at a version it didn't store (broken
  since phase 19); now fixed, with new numbers.
- [x] `checks::InputsChecked` and `check_inputs`; the stale `InputChecker`
  mentions.
- [x] Stale comments and docs, including `consensus_pipeline.md`'s "Not
  covered yet".
- Kept: the perpetual certified-checkpoint queue. It is the tests' only view
  of what anchovy's own aggregator certified: compat's agreement checks and
  the consensus-glue epoch chain. Its comment in `main.rs` now says so.
- Kept: `Store::clear_uncommitted` and `write_through`, and `HandledCommit`'s
  stats fields, all test-only. They cost nothing in production, and gating
  them would add feature plumbing across crates.

## Duplication

- [x] The gate builds each chunk's `ChunkSettlement` from the chunk.
  `GatedChunk` carries `last_of_commit`, `root` and the update. The commit
  stage no longer threads a digests vector beside the entries, and two lint
  suppressions went with it.
- [x] Synced contents are verified once, on fetch, and parsed after that.
  Chunk ends come from the arena.
- [x] `SharedAccess::input_key` is the single mapping from a shared access to
  its key, used by the resolver, settlement and the exec-pool tests.
  Settlement no longer keeps a copy of the base.
- [x] The beacon: the group key is a `BeaconEvent`, emitted on completion and
  on restore. `with_beacon(beacon, party)` replaces `with_beacon` and
  `with_party`. `key_sent`, `send_group_key` and `dkg_status` are removed.
- [x] `Queue::push` replaces six copies of the `push` helper and a dozen
  inline full-queue panics.
- [x] `CommitRecord`, `BuiltCheckpoint` and generation values derive their
  BCS encoding, with the same bytes as before.
- [x] The accumulator root's id is defined once, in `messages::tx_index`.
  The executor family keeps its own, as ported code.
- [x] `EffectsWaitItem::Settlement(SettlementInput)` replaces two variants
  that mirrored it.
- [x] A commit after certificates close clears its observations, JWK votes
  and capabilities once, instead of four checks.
- Not done:
  - **The change-epoch case in `synced::split`.** Its panicking deleter
    states that change-epoch never reads an ended stream; folding it into
    the chunk loop would lose that.
  - **The commit handler builds its estimator twice at start-up.** Passing
    stored observations to `new` would touch every constructor call for
    about five lines.
  - **The last commit index is tracked in three places.** The handler's own
    skip is relied on by its tests.
  - **Builder `Option`s that production always sets.** Requiring them would
    push constructors past the argument limit, and each costs only one
    branch.
  - **`Executable` as an enum of consensus and synced items.** A medium
    change across four crates, worth doing only alongside other changes
    there.
  - **Two low-value items:** `FundsReserver.owners`, and the epoch closer
    seeing every built checkpoint.
