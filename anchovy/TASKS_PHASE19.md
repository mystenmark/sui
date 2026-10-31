# Phase 19: toward a production network

From the production assessment after phase 18b, in the user's order (safe
mode skipped for now):

1. Remote panics and the version over-claim.
2. A node binary: sui's `validator.yaml`, its key files and `genesis.blob`
   (`sui-config` approved).
3. Certified-checkpoint state sync, with a StateSync server.
4. Mainnet replay of the executor.
5. Resubmitting this validator's checkpoint signatures until sequenced.

## Done

- [x] Pings, as the reference answers them: an empty submission is
  acknowledged with its block's ping position, which every committed block
  finalizes; `wait_for_effects` answers executed with the zero effects
  digest, within ten seconds.
- [x] The read RPCs (object, transaction, checkpoint, system state) answer
  `Unimplemented` rather than panicking. In the reference only diagnostic
  tools (sui-tool) call them.
- [x] Only the protocol versions the executor runs are announced and
  started: today only the highest (138), the first with gas model 15.
- [x] Found by the two-sided compat test: after a restart, rounds completed
  just before it were never pushed again, stalling a peer that needed this
  member's signature. Recent rounds' signatures are now pushed again.

- [x] Step 2: `anchovy-node` runs from sui's `validator.yaml`: its keys,
  `genesis.blob` (installed into a fresh database; a database holding
  another chain is refused), `db_path`, the RPC and p2p addresses, and
  consensus parameters. The chain is named by the genesis digest.
  Certified checkpoints are dropped until state sync exists.

- [x] Step 3a: the StateSync server. The running epoch's checkpoint server
  answers sui's five routes from committed checkpoints (new summary- and
  contents-digest indexes; genesis's certified summary installed with
  genesis). A sui full node whose only peer is anchovy syncs and executes
  alike (compat).

## In progress

Step 3, catching up (`docs/state_sync_design.md`: two front ends into one
executor, the chunk lock, a fast path):

- [x] 3b: the checkpoint fetcher, summary verification, and the fast path.
  - `Committee::verify_certified`: epoch, signers, quorum, aggregate.
  - `SyncedScheduler` (crate `synced`, per epoch) chains fetched summaries
    from the committed checkpoint, verifies each, and waits it on
    `CheckpointBuilt(seq)`; it answers the fetcher with what to fetch next,
    within 256 of the committed checkpoint.
  - The built router fork-checks a synced checkpoint and hands it to the
    committer, which commits each checkpoint once, in order, from either
    source: a cut checkpoint commits even if its signatures never arrive.
  - The follower on the p2p endpoint asks committee members' availability
    every 5 s and fetches summaries by sequence number, in batches of 16.
  - Seen in the crash compat test: after the restart, checkpoints the
    reference certified meanwhile come through the fast path. No test
    asserts it yet; 3c's end-of-epoch test depends on it.
- [ ] 3c: the chunk lock, the slow path (synced execution), certified
  randomness into the gate, and the consensus path's checks of
  transactions executed already.
  - [x] The lock (`validator::pipeline::chunk_lock`): the commit stage
    claims each commit's chunks before emitting them and reports its last
    cut; the synced scheduler closes it when dropped.
  - [x] Fetch contents for checkpoints past the cut; verify them against
    the summary; split them into chunks at barriers (the transaction whose
    effects change `0xacc`), numbered on from the cut or the previous
    synced checkpoint.
  - [x] Synced executables (`synced::item`): owned and package keys from
    the transaction, consensus objects at their effects' input versions,
    `Ended` and cancellation markers, the funds verdict from the status,
    the expected effects digest; start versions seeded before executing;
    no generations or funds settles.
  - [x] Wait each checkpoint's transactions executed, check their effects,
    release its chunks, insert it as built, hash it, commit it. The
    commit stage still builds it and compares (`insert_built`); the state
    hasher accepts a checkpoint hashed again alike. Chunks are taken from
    the chunk table as their checkpoint is built, not pruned at commit.
  - [x] Certified randomness bytes to the signature collector
    (`BeaconInput::Certified`), from split checkpoints and from cut ones
    not built yet that list rounds.
  - [ ] The epoch's last checkpoint on the slow path (it takes the fast
    path for now): change-epoch last, the root state for its record.
  - [x] The consensus path's checks of transactions executed already
    (`check_decisions`): shared input versions and stream ends,
    cancellation markers, the funds verdict unless cancelled.
  - [x] Compat: a lagging consensus (`Lagging` holds anchovy's commits).
  - [ ] Compat: catching up an epoch's end; deviation.
  - [x] Consensus output is written once its checkpoints are committed
    and built here (`BuiltCheckpoints::built_through`): before, a synced
    commit wrote output of commits decided but not yet checked, which a
    crash would never re-decide. Backpressure ignores chunks scheduled
    below a committed checkpoint.
  - Known gaps: stream-ended inputs on the synced path are untested.

## Left out / later

- The read RPCs themselves.
- Gas model 14 and older (the executor's legacy path): needed before
  mainnet or testnet reach version 138 to join them.
- Safe mode.
- Pushing certified checkpoints to peers (they poll every 5 s instead).
- Running as a fullnode.
