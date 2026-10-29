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

- [ ] 3b: the checkpoint fetcher, summary verification, and the fast path.
  - [ ] `Committee::verify_certified`: epoch, signers, quorum, aggregate.
  - [ ] `SyncedScheduler` (per epoch): chains fetched summaries from the
    committed checkpoint, verifies each, and waits it on
    `CheckpointBuilt(seq)`; answers the fetcher with what to fetch next.
  - [ ] The built router fork-checks a synced checkpoint and hands it to
    the committer, which commits each checkpoint once, in order, from
    either source: a cut checkpoint commits even if consensus is gone
    before its signatures arrive.
  - [ ] The fetcher on the p2p endpoint: peers' availability every 5 s,
    summaries by sequence number, a bounded window ahead of the committed
    checkpoint.
- [ ] 3c: the chunk lock, the slow path (synced execution), certified
  randomness into the gate, and the consensus path's checks of
  transactions executed already.

## Left out / later

- The read RPCs themselves.
- Gas model 14 and older (the executor's legacy path): needed before
  mainnet or testnet reach version 138 to join them.
- Safe mode.
- Pushing certified checkpoints to peers (they poll every 5 s instead).
- Running as a fullnode.
