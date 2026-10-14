# Writeback cache: design

A from-scratch writeback cache for anchovy. It is the sole access point to
the object database. Transaction outputs live in memory, readable by
everything, until the certified checkpoint containing them commits. Then
they are written in one batch per checkpoint.

References like (R3.4) or (I8) cite `docs/writeback_cache_reference.md`, the
specification of Sui's cache. B-numbers are bugs from its history.

## Goals

1. **Nothing uncertified on disk** (R0.1, I1). Execution outputs reach the
   database only in the commit of a certified checkpoint whose transactions
   have all executed.
2. **One coherent view** (R0.2). Every read sees uncommitted outputs
   first, then committed data. Readers never see:
   - a version going backwards;
   - a value vanishing while it moves to the database;
   - a stale "does not exist";
   - a parent without its children at their versions.
3. **Batched writes.** One database batch per checkpoint, atomic with the
   commit watermark (I17).
4. **Bounded memory** for committed data. Uncommitted data is bounded by
   how far certification lags execution.
5. **Simpler than the reference where anchovy allows it.** Wherever the
   reference coordinates several structures with tickets or lock ordering,
   use one structure under one lock instead.

## What anchovy changes about the problem

The reference's design follows from Sui's architecture (R12). Anchovy's
differs in four ways that remove whole mechanisms.

- **No markers.** Consensus stream ends are generations in the generation
  table (`Generation::Ended`), not markers. A receiving object is checked
  against the object's current version, not against a `Received` marker.
  No transaction sees a version-skipping "fastpath" stream.

  So the cache keeps no marker table, and nothing like smearing (R1.3).
- **No notification registry.** The pipeline's waiters are notified by
  the executor after it writes. A waiter's lookup reads the cache.
  "Visible before notified" holds because the executor writes the cache,
  then resolves keys (R4.9 and R4.10 collapse into the existing waiter
  rule).
- **One writer, one committer.** Only the executor writes outputs. Only a
  new processor, `CheckpointCommitter`, commits, in checkpoint order. Both
  are processors, so "one write per transaction" (I14) and "commit in
  order" (I5) follow from their structure.

  With an executor pool later, writes still come from executions of
  distinct transactions. Per-object version order still follows from the
  ordinals: generation k+1 is written only after generation k's value
  was handed to the transaction that writes it.
- **No pre-consensus owned-object locks.** Locks live in the commit
  handler's own state (phase 11). That matches the reference after
  `22f9fc9781`, so B-12 does not arise.

## Structure

The `store` crate splits in two:
- `store::db` is the database as now: objects, live, transactions,
  effects, executed effects, events, meta and checkpoints. It is private
  to the crate.
- `store::Store` becomes the cache, with the same name and read methods,
  so most callers do not change.

### Objects: one slot per object id

```rust
struct Slot {
    /// Uncommitted versions, oldest first. Never evicted (I2).
    dirty: VecDeque<(u64, Entry)>,
    /// The newest committed versions, oldest first, at most
    /// COMMITTED_VERSIONS. Filled only by commit (I4).
    committed: VecDeque<(u64, Entry)>,
    /// The latest version as last read from the database, or
    /// `NonExistent`. Filled by reads, and only when `dirty` and
    /// `committed` are both empty.
    db_latest: Option<Latest>,
}
enum Entry { Object(Arc<Message<Object>>), Deleted, Wrapped }
enum Latest { Version(u64, Entry), NonExistent }
```

The reference uses three structures for this: `dirty.objects`,
`cached.object_cache` and `object_by_id_cache`. Here they are three fields
of one `Slot` behind one lock, in a sharded map of `Mutex<Slot>`. That
removes several of the reference's hazards by construction:

- **The dirty-to-committed move is atomic.** It is one critical section
  (I7, R3.4). There is no two-lock ordering to keep (R5.2).
- **The latest version is derived, not cached.** It is `dirty.back()`,
  else `committed.back()`, else `db_latest`. A writer updates it by
  pushing to `dirty`. It cannot go backwards (B-14) or go stale after
  eviction (B-18), because there is no second copy to get out of step.
- **`db_latest` is filled only for a slot with no versions in memory**,
  under a ticket (below).
- **Version contiguity (I3) is local**: `committed ++ dirty` is a
  contiguous newest suffix of the object's versions. That gives negative
  and bounded answers from memory (R4.1, R4.3), as in the reference.

**Eviction.** A slot is evictable only when `dirty` is empty, and is
evicted whole (R8.2). Committed slots are kept in a bounded LRU by object
id. Evicting a slot drops its committed versions and its `db_latest`.

**Tickets.** `db_latest` is the only object state filled from a database
read, and a read releases the slot lock while it goes to the database.
The protocol:
1. The reader takes a ticket (a generation number) from a sharded
   generation array, which survives eviction, as in the reference's
   `MonotonicCache` (R5.1).
2. Any write, commit or clear touching the id bumps that generation under
   the slot lock.
3. The reader re-locks and fills `db_latest` only if the generation is
   unchanged and the slot still has no versions.

That is B-18's fix, applied to the one place it is still needed.

### Transactions: one record per transaction

```rust
struct TxOutputs {            // what the executor writes
    digest: TxDigest,
    transaction: Arc<[u8]>,
    effects_digest: [u8; 32],
    effects: Arc<Message<TransactionEffects>>,
    events: Option<Arc<[u8]>>,
    written: Vec<(ObjectId, u64, Arc<Message<Object>>)>,
    removed: Vec<(ObjectId, u64, Removal)>,   // Deleted | Wrapped
}
```

- **Uncommitted:** a sharded map `TxDigest → Arc<TxOutputs>`. Presence
  means "executed": that is the linearization point (I12). The reference
  has four maps for this (R1.4 to R1.7); one record can never be half
  present.
- **Committed:** a bounded LRU `TxDigest → Option<Arc<CommittedTx>>`,
  where `None` is a negative entry. Negative entries are filled under a
  ticket (I10, B-20), and positive ones only by commit.
- **Effects by effects digest:** not cached. Anchovy's readers look
  effects up by transaction digest; a lookup by effects digest goes to
  the database.
- **Empty events and absent events read alike** at every level (R1.7,
  B-34).

## Write path: `Store::write(outputs)` (executor only)

In this order (I11, R2.3):
1. **Tombstones**, for `removed`: push `Deleted` or `Wrapped` to each
   slot's `dirty`, bumping its generation.
2. **Child objects** (owned by an object), then everything else, pushed
   to `dirty`. Bounded child reads (`object_at_or_before`, R4.3) can then
   never pair a parent's new version with a child's stale one.
3. **The transaction record**, inserted into the uncommitted map. From
   here the transaction is executed for every reader.

Each push asserts the version is above the slot's highest (R1.1's
`fatal!`): a duplicate or out-of-order write is a bug, not something to
tolerate (I14, B-31).

The executor resolves its waiter keys only after `write` returns, the
existing rule. Nothing is notified from inside the cache.

## Commit path: `CheckpointCommitter`

A new processor:

```
CheckpointAggregator --certified, in sequence order--> CheckpointCommitter
```

- **Order.** The aggregator certifies in sequence order, as the
  reference's does: a quorum on a later checkpoint waits for the earlier
  ones (I5, B-01). Certificates therefore reach the committer in commit
  order, with no waiter between them. Nothing downstream could use a
  later certificate sooner.
- **What it commits.** The checkpoint's transactions in checkpoint order:
  the order of its contents, which is causal order (R3.1).
- **Steps:**
  1. **Build** one database batch:
     - every transaction's outputs, in order, as `db` writes them today;
     - the checkpoint's `BuiltCheckpoint` record and certified summary;
     - the `highest_committed_checkpoint` watermark.

     A transaction not in the uncommitted map was committed before a
     crash. It is skipped, which makes re-commit idempotent (B-07, I17).
  2. **Write the batch.** It must be durable before anything leaves
     memory (I6, B-03).
  3. **Move each transaction**, in order:
     - each of its object versions, from `dirty` to `committed`, under the
       slot lock. Pop the front of `dirty`, asserting it is this version
       (R3.4's `pop_oldest`); push it to `committed`; truncate to
       `COMMITTED_VERSIONS`; bump the generation;
     - its record into the committed LRU with a write ticket;
     - then drop the record from the uncommitted map.
  4. **Prune** pipeline tables that only uncommitted work needs: chunk
     table entries, and built checkpoints below `seq` in the in-memory
     checkpoint store. A pruned checkpoint still counts as built for the
     waiters, so a late signature for it is released and dropped.

**The checkpoint record moves into the commit batch.** Today
`SummaryBuilder` persists each built checkpoint as soon as it is built.
That is a durable record referring to uncommitted transactions (B-06). It
now goes in the commit batch, so a stored checkpoint always has its
transactions stored with it.

The restart watermark of phase 11 becomes `highest_committed_checkpoint`.
The reference writes it in the same batch as the outputs (B-42). We do
the same, and there is no separate executed watermark to fall out of step
with it (R7.5 item 5): with no checkpoint executor of its own, the
validator's committed checkpoint is its executed one.

**`CheckpointCommitter` is the start of a `CheckpointExecutor`.** The
reference's checkpoint executor also checks local effects against the
checkpoint, runs end-of-epoch work, and is fed by state sync. The committer
takes what that executor takes: certified checkpoints in sequence order,
with transactions from the checkpoint's contents. Feeding it from state
sync instead of the aggregator is the likely end state.

**Checkpoint signatures.** Certification stays its own processor
(`CheckpointAggregator`) behind the summary waiter, as no signature order
is needed. Signatures come from two places, as in the reference
(`consensus_validator.rs`, commit 2dfd307272):
- the block voter, from blocks as it votes on them, for latency;
- the commit decoder, from committed transactions.

Each usually arrives twice. The aggregator ignores a signer it has
already counted before parsing the signature, so the second copy is
nearly free. Pending signatures are not persisted, nor are they in the
reference (its table was removed in adeb47dc85). They come back on restart
because every signature that matters, one for a checkpoint not yet
certified, is in a commit the restart replays:
- now, since replay starts at the epoch's start;
- later, with persisted handler state, only if the persisted consensus
  position never passes a commit holding such a signature. The reference
  gets this from its quarantine flushing commits only up to the highest
  executed checkpoint. Tying the persisted position to the committed
  checkpoint (B-40's rule) gives it too: such a signature is committed
  after the checkpoint it signs was built, so after every commit that
  checkpoint holds.

Signatures seen only in blocks may be lost on restart: each is a copy of a
committed one, or its signer resubmits it.

## Read paths

The lookup order is uncommitted, then committed, then the database, as
in R4. Database errors panic, as reads today do.

| Read | Rule |
|---|---|
| `object(id, version)` | The slot: an exact match is a hit (a tombstone is `None`). A version above the slot's least version that is absent is a negative hit (R4.1). Otherwise the database, not cached (I4). |
| `latest(id)` | Derived from the slot. If the slot is empty: `db_latest`, else the database's `live`, cached into `db_latest` under a ticket. A tombstone is cached as a versioned tombstone, never as `NonExistent` (B-23, B-24). |
| `object_at_or_before(id, bound)` | The highest slot version at or below `bound`, if the slot has one. If not, and the slot's least version exceeds `bound`, the database range scan. If the slot is empty, the database. The reference's assertions about pruned tombstones (B-08, B-11) do not arise: anchovy does not prune yet. Pruning must re-check them. |
| `executed(tx)`, `effects(tx)`, `transaction(tx)`, `events(tx)` | The uncommitted record, then the committed LRU (with a negative hit), then the database, caching a negative under a ticket taken before the lookup (R4.5). Positives from the database are not cached (B-20). |
| balances at a root version | Already capped at the root version by the funds reserver and settlement (`object_at_or_before(account, root_version)`, R4.12 and I21). Still correct through the cache, because settlement writes accounts at the root's version. |

**Packages.** A package is an object with one version, read through
`latest`. The executor already keeps its own package cache
(`PackageCache`). A cache-level package cache is unnecessary for now
(R1.10). If one is added, it must fall back through the slot, not the
database (B-22).

## Epoch boundaries and recovery

**Epoch end.** Every executed transaction came from a consensus commit,
so by the epoch's last certified checkpoint the uncommitted set must be
empty. The reference expects this for everything but owned-only
transactions (I16), and anchovy has none.

Reconfiguration therefore asserts the uncommitted set is empty rather
than dropping it (decided; the reference holds the same), so there is no revert path to get wrong (B-04, B-09,
R6.2). A `clear_uncommitted` exists for restart and tests only. It bumps
each slot's generation under its lock, which closes R6.2's hazard of a
reader re-inserting a dropped version.

**Restart.** The database holds exactly the outputs of checkpoints
`<= highest_committed_checkpoint`. The pipeline must re-derive everything
after it. That is the restart work phase 11 deferred to here, in two
parts:
1. **Replay the epoch's commits from its start** through the commit
   handler. Its state (processed transactions, owned-object locks, the
   prologue's commit timestamps), the resolver's generation bases, and
   the checkpoint numbering are all functions of the commits, so replay
   rebuilds them exactly. A transaction in a committed checkpoint is found
   executed in the database: the executor records its generations from
   its stored effects instead of executing it (phase 11's replay path).
   Checkpoints up to the committed one are rebuilt, identically, but not
   certified again: the aggregator starts after the committed one.
   Everything later is re-executed into the cache.

   Start-up must take nothing from the store's current state, which may
   be ahead of the epoch's start:
   - the accumulator root's version at the epoch's start is committed
     with the genesis checkpoint (with the previous epoch's last, once
     reconfiguration exists);
   - settlement's first root version comes from its first chunk;
   - a replayed transaction's first read of a shared object comes from
     its stored effects, as the object may since be deleted.
2. **Later, if replay from epoch start is too slow:** persist handler
   state at checkpoint boundaries in the commit batch, the analogue of
   the reference's consensus quarantine (R7.4). It must keep B-40's rule:
   persist consensus output only up to a point where every transaction
   it produced is in a committed checkpoint.

Part 1 needs consensus to redeliver the epoch's commits, which
consensus-core does from its own store. Part 2 is an optimization, not a
correctness need, and is not in this phase. Decided: replay from epoch
start for now; it will be replaced by persisted handler state
eventually.

## Backpressure

The uncommitted set grows while certification lags execution. The
reference throttles consensus intake above a pending count, unless
execution has caught up with certification (I22, R8.3). Add the same
later: the commit handler waits while
`uncommitted > threshold && certified > committed`. It is not needed for
correctness, so it is not in the first cut.

## Testing

From R9.4:
- **A model test.** Random writes, commits and evictions against a
  reference model (a `BTreeMap` of everything ever written, plus the
  committed watermark). After every step, every read form gives the
  model's answer.
- **The eviction sweep** (R9.4 `Scenario::iterate`): each scenario rerun
  with every evictable structure dropped after step k, for every k.
- **Race tests:**
  - a reader and a writer on one id, checking the latest version never
    goes backwards (`latest_object_cache_race_test`);
  - a parent read with its child read bounded by it (`test_concurrent_readers`);
  - a negative read racing a commit (`test_transaction_cache_race`).
- **Restart:** a new cache over the same database after a crash between
  the batch write and the in-memory move (B-07), and after one before
  the batch.
- **End to end:** the pipeline tests, with the database inspected to
  hold exactly the committed checkpoints' outputs.

## Order of work

1. `store` split into `db` and the cache: slots, the transaction
   record, reads and writes, with model and race tests. Callers keep
   their `Store` methods.
2. The executor writes through `Store::write`; uncommitted outputs are
   visible everywhere; nothing reaches the database yet.
3. `CheckpointCommitter`, the in-order aggregator, and the commit batch with
   the watermark and the checkpoint record; `SummaryBuilder` stops
   persisting.
4. Restart: replay of the epoch's commits, with the end-to-end restart
   test extended to a crash before and after a commit.
5. Backpressure.
