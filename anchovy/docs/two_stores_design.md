# Two stores

Anchovy keeps state in one Tidehunter database (`store::Store`, with its
writeback cache) and some twenty in-memory structures. Several grow
without bound within an epoch: the commit handler's processed digests and
owned locks, the generation table, the resolver's per-stream state, the
state hashes, the package cache. This design gives the node exactly two
stores, as the reference has (`docs/two_stores_reference.md`), with its
checkpoint and committee databases folded into the perpetual one.

## Perpetual store

`store::Store`, as today, at `<db>/perpetual`: objects, transactions,
effects, events, committed checkpoints and their certified summaries, the
committed watermark, behind the writeback cache. Changes:

- **Epoch records**, `epoch → EpochRecord` (its committee, start, previous
  checkpoint and root), replacing the `meta/epochst` singleton. The current
  epoch is the highest. Writing the next one stays the durable point of
  reconfiguration. This is the reference's committee store,
  `epoch_start_configuration` and `root_state_hash_by_epoch` in one table.
- **Start versions move out**, to the epoch store (below).

Write discipline unchanged: execution writes the cache; the checkpoint
committer writes a certified checkpoint's outputs in one batch.

## Epoch store

A new Tidehunter database per epoch, `<db>/epoch_<N>`, opened by
`Epoch::start` and handed to the pipeline. After reconfiguration has written
the next epoch's record and the old epoch is dropped, its directory is
deleted (keeping the previous one, as the reference keeps a few).

Three sections, each its own keyspaces, each with its own writer type. A
writer is not `Clone`, and is constructed once, when the store opens. Every
section has cloneable readers.

### Consensus output: written only by the commit handler

The `ConsensusOutput` writer is moved into `CommitStage`. Nothing else
can write these keyspaces.

| Keyspace | Key → value | From |
|---|---|---|
| `processed` | transaction digest → () | `CommitHandler.processed` |
| `locks` | object ref → transaction digest | `CommitHandler.locks` |
| `commit` | () → the stage's state after a commit: last commit index, checkpoint queue (pending heights, timestamps, next height and sequence number), commit interval window, closed flag, accumulator root version | `CommitStage`, `CheckpointQueue`, `CommitIntervalObserver`; the reference's `last_consensus_stats_v2` |

The commit handler writes one batch per commit: the commit's new processed
digests and locks, and the `commit` record. Its in-memory state becomes
an overlay over these keyspaces, holding only what is not yet durable.

### Checkpoint builder state

The `CheckpointBuilderTables` writer is shared only by the checkpoint
processors (`SummaryBuilder`, `EpochCloser`, `StateHasher`).

| Keyspace | Key → value | From |
|---|---|---|
| `built` | seq → `BuiltCheckpoint` | `CheckpointStore` (built, not yet committed); the reference's builder summaries and locally computed checkpoints |
| `state_hashes` | seq → ECMH, running root | `StateHashes` |

### Everything else

| Keyspace | Key → value | From |
|---|---|---|
| `start_versions` | (id, initial shared version) → version, digest | the store's `start_versions`, which the executor records at a stream's first use |
| `generations` | (stream, round, k) → generation | `GenerationTable` (see open question 3) |

### Kept in memory

Bounded, or rebuilt by replay, so not stored: the decoded-transaction cache
(bounded by GC depth), the consensus status cache (400 rounds), the
signature caches, settlement and funds state (chunks and reservations in
flight), the chunk table (pruned at each commit), the aggregator's pending
signatures, the waiters and queues, and the executor's type cache. The
package cache becomes bounded.

## Crash consistency: open question 1

Today a restart replays the epoch's commits from its first. The epoch store
can support either:

- **A. Discard and replay.** On restart the epoch's directory is deleted and
  re-created, and the epoch replays from its start as today. The epoch store
  is written eagerly. It bounds memory without touching restart.
- **B. Quarantine and resume** (the reference). The consensus output and
  builder writers keep their batches in memory until a certified checkpoint
  covering them commits. Then they flush through the last commit whose
  checkpoint queue had drained (the reference's `1ad56a6b81`), in one batch
  with the `commit` record. On restart consensus replays from that record's
  commit, and the stages start from the stored state, not the epoch's start.
  The store never holds output of a forked local view.

B is what the strict separation is for. But it changes how every stage
starts: settlement, funds, the resolver and the state hasher must resume
mid-epoch from stored state too.

## Open questions

1. **Restart**: A or B (above). A recommended first, as its own step, with B
   to follow. Most of the work (the stores, the writer split) is common.
2. **The resolver's per-stream state** (`VersionResolver.last_generation`)
   is a function of the commit handler's output, but it is computed
   downstream. Either the commit handler numbers generations itself, which
   makes it consensus output written by the handler, or it stays the
   resolver's, in "everything else". The first is cleaner under B.
3. **Prune rather than store**: the generation table only needs entries a
   transaction not yet executed can read, and the state hashes only the
   running root once a checkpoint has committed. Pruning them at checkpoint
   commit keeps them in memory and off the input waiter's hot path.
   Recommended: prune both; the `generations` keyspace then goes.

## Order of work

1. The epoch store crate or module: Tidehunter per epoch directory, the
   three sections and their writer and reader types, opening and deletion.
2. Perpetual epoch records, replacing `meta/epochst`; start versions moved to
   the epoch store.
3. Consensus output: processed digests, locks and the `commit` record through
   the commit handler's writer.
4. Builder state: built checkpoints and state hashes.
5. Pruning (question 3) and a bounded package cache.
6. If B: quarantine, flush at checkpoint commit, resume on restart.
