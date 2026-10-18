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

A directory per epoch, `<db>/epoch_<N>`, opened by `Epoch::start` and handed
to the pipeline; after reconfiguration has written the next epoch's record
and the old epoch is dropped, epochs before the previous one are deleted.
Nothing in it is pruned: it is dropped with the epoch.

It holds two Tidehunter databases: `replayed`, what a replay of the epoch
writes again, which a restart empties (decision 1, step one); and `kept`,
what a replay could not rebuild, as the stores it was derived from have
moved on. Within them, three sections, each its own keyspaces and its own
writer.

### Consensus output: written only by the commit handler

The `ConsensusOutput` writer is made once, when the store opens, cannot be
cloned, and is moved into `CommitStage`. The handler reads its pending
writes, then the database, and writes one batch per commit.

| Keyspace | Key → value | From |
|---|---|---|
| `processed` | transaction digest → () | `CommitHandler.processed` |
| `locks` | object ref → transaction digest | `CommitHandler.locks` |
| `streams` | stream → its last generation (round, k) | `VersionResolver.last_generation`, moved (decision 2) |

Step two adds `commit`: the stage's state after a commit (last commit index,
checkpoint queue, commit interval window, closed flag, accumulator root
version), the reference's `last_consensus_stats_v2`.

### Checkpoint builder state

The `BuilderOutput` writer is made once, when the store opens, and given
only to the checkpoint builder's parts: the checkpoint store (written by
`SummaryBuilder`, and seeded with the previous epoch's last checkpoint) and
the state hashes (written by `StateHasher`).

| Keyspace | Key → value | From |
|---|---|---|
| `built` | seq → `BuiltCheckpoint` | `CheckpointStore`; the reference's builder summaries and locally computed checkpoints |
| `checkpoint_hashes` | seq → ECMH | `StateHashes` |
| `roots` | seq → running root | `StateHashes` |

### Everything else

| Keyspace | Key → value | From |
|---|---|---|
| `generations` | (stream, round, k) → generation | `GenerationTable`, written by `Executor` |
| `start_versions` (in `kept`) | (id, initial shared version) → version, digest | the store's `start_versions`, which the executor records at a stream's first use |

### Kept in memory

Bounded, so not stored: the decoded-transaction cache (bounded by GC
depth), the consensus status cache (400 rounds), the signature caches,
settlement and funds state (chunks and reservations in flight), the chunk
table (pruned as checkpoints commit), the aggregator's pending signatures,
the waiters and queues. The executor's
Move runtime keeps every package it resolves, so it is replaced, with the
package and type caches filled from it, once it has resolved 1024.

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
3. **Where state lives.** Bounded state stays in memory, pruned as it
   always was. Epoch state that grows without bound (generations, built
   checkpoints, state hashes) goes in the epoch store, whole: it is dropped
   with the epoch, so nothing in it is pruned. Epoch state never goes in
   the perpetual store.

## Order of work

1. The epoch store: per-epoch directory, the sections and their writers,
   opening, deletion, emptying on restart.
2. Perpetual epoch records, replacing `meta/epochst`; start versions moved to
   the epoch store.
3. Consensus output through the commit handler's writer; the round links
   moved from the resolver.
4. Generations and builder state in the epoch store (decision 3); the Move
   runtime bounded.
5. Quarantine, flush at checkpoint commit, resume on restart, with the
   `commit` record.
