# Quarantine and resume

Step two of `docs/two_stores_design.md`. Today a restart empties the epoch
store and replays the epoch's commits from its first. This design follows
the reference (`docs/two_stores_reference.md`, "Restart"). It keeps the
epoch store's consensus output in memory, in a quarantine, until the
checkpoint covering it has committed, then writes it. On restart, consensus
replays from the last commit written, and every stage starts from that
point.

The replay machinery is already there. A restart now replays from the
epoch's start, which is the simplest flushed boundary: no output and no
pending checkpoint. Every stage already handles replayed commits whose
transactions have partly executed or committed. The executor skips the
committed ones and re-publishes their generations and fund settlements from
their stored effects. The aggregator starts after the committed checkpoint.
The builder rebuilds checkpoints already committed. What changes is the
starting point, and the state each stage must find at it.

## The quarantine (`ConsensusOutput`)

As the reference's `ConsensusOutputQuarantine` (cq:488-677).

- **Per commit,** the commit handler builds one output. It holds the
  transactions processed, the owned locks taken, the streams' last
  generations, and the commit record (below). It also carries the
  commit's last checkpoint height, and whether the checkpoint queue was
  drained after it (`checkpoint_queue_drained`, cq:100-102). Outputs queue
  in commit order.
- **Reads** look in the quarantine, then the database. The quarantine
  keeps the latest unflushed value of each key, reference-counted by the
  outputs that wrote it, as the reference's `RefCountedHashMap`
  (cq:988-1049). A key leaves memory when the last output that wrote it
  is flushed.
- **Flush.** When checkpoint `N` commits, the quarantine writes every queued
  output whose height is at most `N`'s last height, through the last of
  them whose checkpoint queue was drained, in one batch (cq:595-676). Later
  outputs stay: their roots are pending checkpoints, which only a replay
  from a drained point rebuilds (`1ad56a6b81`). Pushing an output also
  flushes, as far as the committed checkpoint allows (cq:556-558): after a
  restart, replayed outputs at or below it flush as soon as they are
  pushed.
- **Only the commit handler writes it.** The writer is made once with the
  store, is not `Clone`, and is moved into `CommitStage`.

### The commit record (`commit`)

The reference's `last_consensus_stats_v2` (aes:278-289), in every flushed
batch (cq:301-309):

| Field | Restores |
|---|---|
| last commit index, leader round | where replay starts; commits at or below are prior commits (below) |
| checkpoint height | `CheckpointQueue.height` |
| next checkpoint sequence number | `CheckpointQueue.next_seq` |
| last checkpoint flush timestamp | `CheckpointQueue.last_built_timestamp`, so interval cuts replay alike |
| accumulator root version | `CommitStage.root_version` |
| closed | the epoch's last commit is decided |

At a drained point the checkpoint queue has nothing pending, so these
fields are all of it. Without a record (the epoch's first restart before
any flush), the stage starts from the epoch's start, as today.

### Prior commits

The commit interval observer is in memory, and its digest goes into every
prologue. As in the reference (ch:212-219, 1024-1044), consensus
replays a fixed window of commits before the last one flushed: the
protocol's commit rate window. Commits at or below the record's index only
feed the observer. Later ones are handled in full. A wrong reconstruction
forks at once, through the prologue.

## The rest of the epoch store

| Keyspace | Section | Written | On restart |
|---|---|---|---|
| `processed`, `locks`, `streams`, `commit` | consensus output | quarantine flush | read through the quarantine |
| `start_versions` | everything else | directly, at a stream's first use (as `next_shared_object_versions` init, aes:2254-2256) | kept: the first assignment's live version, from before any write the epoch made |
| `built` | builder | directly, as built (`locally_computed_checkpoints`, cp:1850-1860) | kept: a rebuilt checkpoint must have the same digest, else a fork panics (cp:1816-1840) |
| `checkpoint_hashes`, `roots` | builder | directly (`state_hash_by_checkpoint`, `running_root_state_hash`, aes:1376-1410) | entries above the committed checkpoint deleted at startup (cp:3014-3049) |
| `generations` | everything else | with the producing transaction's checkpoint commit (below) | flushed rounds read back; later ones re-recorded by replay |

The epoch's directory is no longer emptied on restart, so the `replayed`
and `kept` databases become one, as the reference's epoch database is.

## Generations: written with their checkpoint

The reference has no generation table: its consensus handler assigns
versions, which are consensus output. Anchovy's handler assigns ordinals.
The executor records the version each produced. So a generation is an
execution output, like the object it names, and it reaches the epoch
store when that object reaches the perpetual store.

- The generation table keeps what the executor records in memory, by
  producing transaction. Readers look in memory, then the epoch store.
- When a checkpoint commits, the committer writes its transactions'
  generations in the epoch flush batch.
- Generation 0 recorded from a start version belongs to no transaction. It
  is written directly, as the start version is.

After a crash the epoch store holds the generations of committed
transactions only, so none names an object the store lacks. Replay
re-records the rest: committed transactions are re-published from their
effects, others executed again. A later round's base generation, from a
flushed round, is read from the store.

## Order of a checkpoint commit

As the reference's `execute_checkpoint` (cpe:415-523):

1. The perpetual batch: the checkpoint's outputs, its record and
   certificate, the committed watermark. This is today's commit.
2. The epoch batch: the quarantine's flushable outputs, with their
   `commit` record, and the checkpoint's generations.

The perpetual commit comes first. A crash between the two batches leaves
the commit record behind the committed checkpoint. Replay then rebuilds the
committed checkpoints from consensus: the executor skips their
transactions, the builder checks their digests against `built`, the
aggregator does not certify them again, and their outputs flush when
pushed. The other order would let the epoch store claim commits whose
outputs a crash lost, and replay would never regenerate them.

## Each stage on restart

| Stage | Starts from | Reference |
|---|---|---|
| consensus | the record's commit index, less the prior window | cm:345-352 |
| `CommitStage` | the record, else the epoch's start | ch:807-853 |
| `CommitHandler` | processed, locks and streams through the quarantine | cq:780-825 |
| resolver | no state | |
| executor | generations from the store; committed transactions skipped and re-published | esi:560-574 |
| settlement, and the chunk table | empty: replay starts at a drained point, so the first chunk opens a checkpoint, at offset 0. The chunk table is in memory, as the reference's pending checkpoints are (`62091e97ff`) | ss:140-147 |
| funds | the first chunk's version as settled, its balances read at their versions | esi:169-193 |
| contents and summary builders | the checkpoint before the first pending one, from the perpetual store (committed) | cp:1883-1905 |
| state hasher | the root after that checkpoint, from `roots`, else the epoch record's | gsh:506-533 |
| aggregator | the committed checkpoint + 1 (today) | cp:2508-2518 |
| commit stage's status cache, waiters, queues | empty | |

## Departures from the reference

- **No builder summary table.** The reference flushes builder summaries
  (`builder_checkpoint_summary_v2`) with the outputs and resumes the builder
  from the last one. Anchovy commits a checkpoint's record with its outputs,
  in one perpetual batch. So committed checkpoints are that table, and the
  reference's two-watermark cases (cp:3017-3022) do not arise.
- **Generations**, above.

## Order of work

1. The quarantine: per-commit outputs, reference-counted reads, the
   commit record, flush at checkpoint commit, the drained rule.
2. The generation table written with its checkpoint.
3. The perpetual-then-epoch commit order in `CheckpointCommitter`.
4. Restart: no emptying; one database per epoch; startup deletion of state
   hashes above the committed checkpoint; consensus replay from the record,
   with the prior window; each stage started as above.
5. Tests: a crash at each point of a checkpoint commit (before the perpetual
   batch, between the batches, after both), and a crash with outputs
   queued behind an undrained commit, each replay checked to rebuild the
   same checkpoints and certify on.
