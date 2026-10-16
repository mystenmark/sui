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

On restart (decision 1, step one) the directory is deleted and re-created.
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
| `streams` | stream → its last generation (round, k) | `VersionResolver.last_generation`, moved (decision 2) |
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
| `generations` | (stream, round, k) → generation | each stream's last generation pruned from the `GenerationTable` (decision 3) |

### Kept in memory

Bounded, or rebuilt by replay, so not stored: the decoded-transaction cache
(bounded by GC depth), the consensus status cache (400 rounds), the
signature caches, settlement and funds state (chunks and reservations in
flight), the chunk table (pruned at each commit), the aggregator's pending
signatures, the waiters and queues, and the executor's type cache. The
package cache becomes bounded.

## Decisions

1. **Restart: discard and replay first, then quarantine and resume.** Step
   one writes the epoch store eagerly. On restart the epoch's directory is
   deleted and re-created, and the epoch replays from its start, as today.
   This bounds memory without touching restart. Step two is the
   reference's quarantine. The consensus output and builder writers keep
   their batches in memory until a certified checkpoint covering them
   commits. They then flush through the last commit whose checkpoint queue
   had drained (the reference's `1ad56a6b81`), in one batch with the
   `commit` record. On restart, consensus replays from that commit and every
   stage starts from stored state, so the store never holds output of a
   forked local view.
2. **The commit handler links rounds.** A shared access's ordinal is
   (round, writes before it in the round). Generation 0 of a round also
   needs the stream's last generation from an earlier round. The resolver
   keeps that today (`VersionResolver.last_generation`). It moves into the
   commit handler, which attaches it to each access, so it becomes consensus
   output (a `streams` keyspace: stream → its last generation) and the
   resolver keeps no state.
3. **Prune rather than store** the generation table and the per-checkpoint
   state hashes, at each checkpoint commit. Every round before the
   committed checkpoint's last chunk's has executed, so the generation table
   drops those rounds. Each stream's last generation among them is still
   read by later rounds until the stream is written again, and one per
   stream touched in the epoch is unbounded, so it moves to the epoch store
   (`generations`, in "everything else"). Lookups for pruned rounds read it
   there, off the hot path.

## Order of work

1. The epoch store: Tidehunter per epoch directory, the three sections and
   their writer and reader types, opening, deletion, discard on restart.
2. Perpetual epoch records, replacing `meta/epochst`; start versions moved to
   the epoch store.
3. Consensus output: processed digests, locks and the `commit` record through
   the commit handler's writer; the round links moved from the resolver.
4. Pruning (decision 3), and the Move runtime replaced, with the package and
   type caches filled from it, once it has resolved 1024 packages: a
   runtime keeps every package it resolves.
5. Quarantine, flush at checkpoint commit, resume on restart, with builder
   state: built checkpoints, which until then are bounded in memory by the
   window between building and committing, and the `commit` record.
