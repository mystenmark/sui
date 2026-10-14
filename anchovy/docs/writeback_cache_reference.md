# Sui Writeback Cache: Behavioral Reference

This is a behavioral specification of Sui's `WritebackCache`, the execution cache in
`crates/sui-core`. It is written so that the cache can be re-implemented from scratch in another
codebase. Each numbered point gives the required behavior, then the reason for it, with
`file:line` references (relative to the repo root at the commit this was written against,
`f6904e6883`) or commit hashes.

Abbreviations used throughout:

- **WBC**: `crates/sui-core/src/execution_cache/writeback_cache.rs`
- **EC**: `crates/sui-core/src/execution_cache.rs` (the traits)
- **CT**: `crates/sui-core/src/execution_cache/cache_types.rs`
- **AS**: `crates/sui-core/src/authority/authority_store.rs`
- **AST**: `crates/sui-core/src/authority/authority_store_tables.rs`
- **CE**: `crates/sui-core/src/checkpoints/checkpoint_executor/mod.rs`
- **TO**: `crates/sui-core/src/transaction_outputs.rs`
- **CQ**: `crates/sui-core/src/authority/consensus_quarantine.rs`
- **AUTH**: `crates/sui-core/src/authority.rs`
- **TESTS**: `crates/sui-core/src/execution_cache/unit_tests/writeback_cache_tests.rs`
- **NRTESTS**: `crates/sui-core/src/execution_cache/unit_tests/notify_read_input_objects_tests.rs`

"Dirty" means an execution output that has not yet been written to the database. "Committed"
means it has been written to the database. "Cached" means a committed value that is also held in
memory, in a bounded, evictable structure.

---

## 0. Purpose, and what problem the cache solves

**0.1 Required behavior.** Transaction execution writes its outputs (objects, tombstones,
markers, the transaction, effects, events, and so on) to the cache **only**. Nothing produced by
execution reaches the database until the transaction appears in a **certified checkpoint** that
the checkpoint executor has finished executing. At that point the outputs of every transaction in
the checkpoint are written in a single atomic database batch, and only then are they moved from
the dirty set to the evictable cached set (WBC:4-38, CE:415-466).

**Why.** Before the writeback cache, each executed transaction was committed to RocksDB
immediately (the "passthrough" design; see 11.A). That had two costs:

1. One RocksDB write batch per transaction, on the critical path of execution.
2. Outputs that were not yet final were on disk. On a validator, which executes transactions
   before the checkpoint containing them is certified, uncertified state on disk has to be
   reverted at epoch boundaries (the old `revert_state_update` path) and complicates crash
   recovery.

With the writeback cache, the database only ever contains outputs of transactions in certified,
executed checkpoints (plus a few explicit exceptions; see 7.1). Everything else lives in memory
and is lost on a crash, which is acceptable because it can be regenerated: certified
checkpoints are re-executed by the checkpoint executor, and on validators the consensus commits
that have not yet been fully checkpointed are replayed (7.3).

**0.2 Required behavior.** All reads used by execution, scheduling, signing and RPC go through the
cache, which presents a single coherent view: dirty data first, then the committed cache, then the
database. A reader must never observe:

- a version that "goes backwards" (an older latest version after a newer one was visible);
- a value that temporarily disappears while it moves from dirty to committed;
- a parent object at version V without all of its children at their versions <= V;
- a stale negative answer (claiming a thing does not exist when it does).

The rest of this document specifies how each of these is guaranteed.

---

## 1. Data model

The cache has three layers (WBC:211-460):

1. `dirty: UncommittedData`: unbounded, never evicted; entries are removed only by commit or by
   the end-of-epoch clear.
2. `cached: CachedCommittedData`: bounded caches of committed data, with automatic eviction
   (moka segmented caches, 8 segments).
3. Cross-cutting caches that hold both dirty and committed data and are kept coherent on every
   write: `object_by_id_cache` (latest version per object) and `packages`.

Plus notification registries (`executed_effects_digests_notify_read`, `object_notify_read`), the
`store: Arc<AuthorityStore>` handle, and backpressure state.

### 1.1 Objects by (id, version)

**Dirty form.** `dirty.objects: DashMap<ObjectID, CachedVersionMap<ObjectEntry>>` (WBC:226), with
2048 shards (WBC:253). `ObjectEntry` is `Object(Object) | Deleted | Wrapped` (WBC:115-120). Dirty
holds both live objects and tombstones, because tombstones are also flushed to the db (the db
keeps tombstones so that historical queries on fullnodes work, WBC:217-219).

**Committed (cached) form.** `cached.object_cache: MokaCache<ObjectID,
Arc<Mutex<CachedVersionMap<ObjectEntry>>>>` (WBC:325), capacity `object_cache_size` (default
`max_cache_size` = 100,000 entries; `crates/sui-config/src/node.rs:715-750`). Each per-object
version map holds at most `MAX_VERSIONS = 3` versions, the newest three committed versions
(WBC:1304, 1313-1315).

**`CachedVersionMap<V>`** (CT:24-118) is a `VecDeque<(SequenceNumber, V)>` with these rules:

- `insert(version, v)` requires `version` to be strictly greater than the current highest version;
  otherwise it calls `fatal!` ("version must be monotonically increasing") (CT:50-62).
- `pop_oldest(version)` pops the front entry and asserts that its version equals `version`
  ("version must be the oldest in the map"). An assertion failure here means data is being
  committed out of causal order (CT:110-117).
- `truncate_to(n)` drops from the front (oldest) until at most `n` entries remain (CT:103-108).
- `get(version)` is an exact match; `get_highest`, `get_least`, `get_prior_to(v)` (strictly less
  than) and `all_versions_lt_or_eq_descending(v)` are the query forms.

**The "no gaps" property** (WBC:16-38, 220-225). For each object, the dirty queue and the cached
queue together hold a contiguous suffix of that object's version history:

- versions enter the dirty queue only in increasing order (one transaction writes one version of
  an object, and a transaction that writes version V+k is causally after the one that wrote V);
- versions leave the dirty queue only from the front (oldest first), and only by moving to the
  back of the cached queue;
- the cached queue is truncated only from the front.

Therefore every version in the cached queue is older than every version in the dirty queue, and
there is no version of the object anywhere (memory or db) that is newer than the least version
in memory but missing from memory. Either queue may be absent: there may be no dirty versions,
or the cached queue may have been evicted. This allows:

- **negative hits**: a query for version `v` that is not in a queue but is greater than that
  queue's least version is known not to exist anywhere;
- **bounded reads** ("highest version <= bound") answered from memory when any version within the
  queue is <= bound.

**Never populate `object_cache` from a db read.** `object_cache` is written only by commit
(`move_version_from_dirty_to_cache`, WBC:1294-1329). Inserting a version read from disk could
create a gap (e.g. the db read returns v3 while v5 is cached), breaking the negative-hit logic.
This is why the separate `object_by_id_cache` exists (WBC:431-437).

### 1.2 Latest object per id: `object_by_id_cache`

`object_by_id_cache: MonotonicCache<ObjectID, LatestObjectCacheEntry>` (WBC:437), capacity
`object_by_id_cache_size` (default = object cache size). `LatestObjectCacheEntry` is
`Object(SequenceNumber, ObjectEntry) | NonExistent` (WBC:174-178); `ObjectEntry` may itself be a
tombstone. So this cache stores both positive entries and two kinds of negative entry: "latest is
a tombstone at version V" and "no version of this object has ever existed".

It holds **both dirty and committed** values:

- every dirty write updates it with `Ticket::Write` (WBC:633-645);
- reads from the db populate it with a read ticket (WBC:890-922, 1331-1353);
- `find_object_lt_or_eq_version` populates it from the dirty highest or the db latest
  (WBC:1835-1900).

Ordering is enforced with `IsNewer` (WBC:197-207): `Object(v1) > Object(v2)` iff `v1 > v2`, and
any `Object` is newer than `NonExistent`. `MonotonicCache::insert` refuses to replace an entry with
an older one (CT:303-314). Because it contains dirty data, it must be invalidated for any dirty
data that is discarded (6.2), and in debug builds every read checks it against the dirty set or
the db (9.3).

### 1.3 Markers

Markers record per-epoch facts about object versions that cannot be derived from the objects
table. `MarkerValue` (in `sui-types/src/storage`) has these variants:

- `Received`: an owned object was received at this version. It prevents receiving the same
  object at the same version twice (`get_object_received_at_version` treats a received object as
  not existing, EC:751-785).
- `FastpathStreamEnded`: an address-owned object stopped existing at this version: it was
  deleted or wrapped while not consensus-owned, or it was transferred to consensus. Used for
  receiving-object availability (EC:294-309, 402-413).
- `ConsensusStreamEnded(TransactionDigest)`: a consensus (shared or ConsensusAddressOwner) object
  stream ended at this version (deleted, wrapped, transferred out of consensus, or its owner
  changed), together with the digest of the transaction that ended it. These markers are
  **smeared**: the marker is also written at the lamport version of every later transaction that
  was assigned this object mutably after the stream ended (TO:124-144), so that later
  transactions sequenced against the removed object can become ready (EC:310-317) and execution
  can tell that the input was deleted.

Key: `MarkerKey = (EpochId, FullObjectID)` with the version inside the version map (WBC:209).
`FullObjectID` distinguishes `Fastpath(id)` from `Consensus((id, initial_shared_version))`, so an
object that goes address-owned → consensus → address-owned has separate marker streams.

**Dirty form.** `dirty.markers: DashMap<MarkerKey, CachedVersionMap<MarkerValue>>` (WBC:232).
**Committed form.** `cached.marker_cache: MokaCache<MarkerKey,
Arc<Mutex<CachedVersionMap<MarkerValue>>>>` (WBC:328), capacity `marker_cache_size` (default =
object cache size), at most 3 versions per key. **DB form.**
`perpetual_tables.object_per_epoch_marker_table_v2: DBMap<EpochMarkerKey, MarkerValue>` where
`EpochMarkerKey(epoch, FullObjectKey)` (AST:135-138, AS:766-771).

Markers use exactly the same dirty/cached FIFO design and the same negative-hit logic as objects
(WBC:38, 844-888). Because the epoch is part of the key, markers from previous epochs are never
consulted, and the committed `marker_cache` does **not** need to be cleared at an epoch boundary.
The db marker table is fully deleted at reconfiguration (AS:183-208, WBC:1393-1396).

Writing a `ConsensusStreamEnded` marker notifies `object_notify_read` for
`InputKey::VersionedObject{id: marker's FullObjectID, version}` (WBC:677-689): a transaction
waiting for a consensus object at a version that will never be created (because the stream
ended) must become ready so that it can execute against the "deleted" input. Other marker kinds do
not notify; receiving-object waiters are woken through other paths (4.10).

### 1.4 Transactions

- **Dirty:** inside `dirty.pending_transaction_writes: DashMap<TransactionDigest,
  Arc<TransactionOutputs>>` (WBC:244). That map is the authoritative "pending commit" set: one entry
  per executed but uncommitted transaction.
- **Committed cache:** `cached.transactions: MonotonicCache<TransactionDigest,
  Option<Arc<VerifiedTransaction>>>` (WBC:330), capacity `transaction_cache_size` (default
  100,000). Negative entries (`None`) are allowed.
- **DB:** `perpetual_tables.transactions` (AS:754-758).

### 1.5 Effects

- **Dirty:** `dirty.transaction_effects: DashMap<TransactionEffectsDigest, TransactionEffects>`
  (WBC:234).
- **Committed:** `cached.transaction_effects: MonotonicCache<TransactionEffectsDigest,
  Option<Arc<TransactionEffects>>>`, capacity `effect_cache_size`.
- **DB:** `perpetual_tables.effects` (keyed by effects digest).

### 1.6 Executed effects digests (tx digest → effects digest)

This mapping is the definition of "transaction T has been executed locally".

- **Dirty:** `dirty.executed_effects_digests: DashMap<TransactionDigest,
  TransactionEffectsDigest>` (WBC:240).
- **Committed:** `cached.executed_effects_digests: MonotonicCache<TransactionDigest,
  Option<TransactionEffectsDigest>>`, capacity `executed_effect_cache_size`.
- **DB:** `perpetual_tables.executed_effects` (AS:749-752).
- **Notification:** `executed_effects_digests_notify_read: NotifyRead<TransactionDigest,
  TransactionEffectsDigest>` is fired on the dirty write (WBC:1036-1037).

### 1.7 Events

- **Dirty:** `dirty.transaction_events: DashMap<TransactionDigest, TransactionEvents>` (WBC:236).
  An empty events struct is **stored** for transactions without events, to avoid special cases
  during commit (WBC:1017-1023).
- **Committed:** `cached.transaction_events: MonotonicCache<TransactionDigest,
  Option<Arc<TransactionEvents>>>`.
- **DB:** `perpetual_tables.events_2` keyed by transaction digest, written **only** when
  `effects.events_digest().is_some()` (AS:791-797).
- **Read rule:** an empty events struct is translated to `None` on every read path (WBC:2264-2270),
  so dirty, cached and db reads agree.

### 1.8 Unchanged loaded runtime objects

- **Dirty:** `dirty.unchanged_loaded_runtime_objects: DashMap<TransactionDigest, Vec<ObjectKey>>`
  (WBC:238).
- **No committed cache:** reads fall back directly to the db (WBC:2331-2362).
- **DB:** `perpetual_tables.unchanged_loaded_runtime_objects`, written only if the list is
  non-empty (AS:799-805). A db miss and an empty list are therefore indistinguishable to readers
  after commit.

### 1.9 Executed-in-last-epoch (replay protection)

`cached.transaction_executed_in_last_epoch: MonotonicCache<(EpochId, TransactionDigest),
Option<()>>` (WBC:340-341), backed by `perpetual_tables.executed_transaction_digests:
DBMap<(EpochId, TransactionDigest), ()>`, which is written in every commit batch (AS:760-763,
AST:140-143). It answers "was transaction T executed in epoch E-1?" (WBC:2210-2246) and caches both
answers. `write_transaction_outputs` asserts that the answer is false (WBC:940-945). This is
replay protection for transactions that can span two epochs (address-balance gas payments with
multi-epoch expiration, commit `25d30f6064`).

The cache key includes the previous epoch, so the cache never needs invalidation: once epoch E
starts, the set of transactions executed in E-1 is final and fully committed (6.1).

### 1.10 Packages

`packages: MokaCache<ObjectID, PackageObject>` (WBC:449), capacity `package_cache_size` (default
1000). Packages are immutable and have exactly one version per id, so the version-contiguity rule
does not apply and they can be inserted from either a write or a db read (WBC:439-448). It holds
**dirty and committed** packages. It is populated on write (WBC:986-990) and on a read miss
(WBC:1582-1591). On a miss, the lookup goes through `get_object_impl`, which checks the dirty set
before the db; this is required because a dirty package can be evicted from `packages` before it is
committed (WBC:1579-1581, commit `205ef11e0a`). Non-final packages are removed at epoch end
(WBC:1374-1378).

`force_reload_system_packages` is a no-op (WBC:1605-1608): every write goes through the cache, so
the package cache cannot become incoherent. Under the old passthrough cache, this method was
needed after system package upgrades.

### 1.11 Owned-object locks

`ObjectLocks` (`execution_cache/object_locks.rs`) is now **stateless**. Pre-consensus owned-object
locking was removed (commits `22f9fc9781` "Disable Owned Object Locking", `10436c976a`).
`clear()` is a no-op (object_locks.rs:34-37). What remains is
`validate_owned_object_versions(refs)` (object_locks.rs:92-108): it reads the latest version of
each object through the cache (`multi_get_objects` → `get_object`, so dirty data is visible) and
requires that each object exists, that `obj_ref.1 == live.version()`, and that
`obj_ref.2 == live.digest()`. It is used to validate inputs before signing and voting, and takes
no locks.

Owned-object locking now happens **post-consensus**, in the epoch store
(`owned_object_locked_transactions` table), and is quarantined with the consensus output
(`ConsensusOutputQuarantine.owned_object_locks`, CQ:509-510, 742-753, 803-826). That is outside
the cache. The history of the in-cache lock table (in memory, then removed) is in section 11.

### 1.12 Other state

- `cached._transaction_objects: MokaCache<TransactionDigest, Vec<Object>>` (WBC:343-345) is
  unused (reserved for signing-time object reuse) but still bounded and cleared.
- `UncommittedData.total_transaction_inserts` and `total_transaction_commits` are atomic counters
  used for backpressure (WBC:246-247, 1043-1054, 1126-1139).
- `TransactionOutputs.accumulator_events: Mutex<Option<Vec<AccumulatorEvent>>>` (TO:22) can be
  taken exactly once via `take_accumulator_events` while the transaction is still dirty
  (WBC:2364-2369, TO:176-181). Taking it twice panics. After commit the cache returns `None` and the
  caller recomputes the events from the effects (accumulators/mod.rs:243-250).

### 1.13 Summary table

| Data | Dirty | Committed cache (bound) | Negative caching | DB table |
|---|---|---|---|---|
| object versions | `DashMap<id, VersionMap>` | moka `id → VersionMap` (N entries, ≤3 versions each) | implicit, via version contiguity | `objects` |
| latest object | (shared) `object_by_id_cache` | same | `NonExistent`, tombstones | `objects` (reverse scan) |
| markers | `DashMap<(epoch,id), VersionMap>` | moka (≤3 versions) | implicit, via version contiguity | `object_per_epoch_marker_table_v2` |
| tx | `pending_transaction_writes` | `MonotonicCache<Option>` | yes | `transactions` |
| effects | `DashMap` | `MonotonicCache<Option>` | yes | `effects` |
| executed digest | `DashMap` | `MonotonicCache<Option>` | yes | `executed_effects` |
| events | `DashMap` (empty stored) | `MonotonicCache<Option>` | yes | `events_2` (non-empty only) |
| unchanged loaded objs | `DashMap` | none | no | `unchanged_loaded_runtime_objects` (non-empty only) |
| executed in last epoch | n/a | `MonotonicCache<(E,tx), Option<()>>` | yes | `executed_transaction_digests` |
| packages | (shared) `packages` | same (1000) | no | `objects` |
| owned locks | none (moved to epoch store) | | | epoch store |

---

## 2. Write path: `write_transaction_outputs(epoch, Arc<TransactionOutputs>)`

### 2.1 Inputs

`TransactionOutputs` is built by `TransactionOutputs::build_transaction_outputs` (TO:30-174) from
the effects and the execution's temporary store:

- `written`: map of every object written (created, mutated, unwrapped), at its new version.
- `deleted`: `effects.deleted() ∪ effects.unwrapped_then_deleted()` as `ObjectKey(id, version)`,
  where the version is the transaction's lamport version.
- `wrapped`: `effects.wrapped()`.
- `markers`, built in this order: received objects (`Received`, using the receiving ref's
  version); every tombstone (`ConsensusStreamEnded(tx)` if the input was a consensus object,
  else `FastpathStreamEnded`); objects transferred to consensus (`FastpathStreamEnded`); objects
  transferred out of consensus or whose consensus owner changed (`ConsensusStreamEnded(tx)`); and
  smears for stream-ended, mutably-accessed consensus inputs at the lamport version.
- `transaction`, `effects`, `events`, `unchanged_loaded_runtime_objects`, and
  `accumulator_events`.

### 2.2 Preconditions (caller obligations)

1. **At most one write per transaction.** The same transaction must not be written twice unless
   the cache was cleared in between. The caller enforces this: `try_execute_immediately` takes a
   per-digest mutex (`acquire_tx_guard`) and returns early if `get_executed_effects(tx)` is already
   `Some` (AUTH:1555-1569). A duplicate write would hit `fatal!` in `CachedVersionMap::insert`,
   because the same object version would be inserted twice.
2. **Causal order of writes per object.** A transaction that writes version V of an object must
   not be written before the one that wrote that object's previous version. This follows from
   execution scheduling: a transaction runs only when its inputs at the assigned versions are
   available (4.10).
3. **Execution lock held for read.** The writer holds `ExecutionLockReadGuard` for the current
   epoch (AUTH:1580-1594), which excludes reconfiguration (6.1).
4. **Not executed in the previous epoch.** Asserted inside the call (WBC:940-945).
5. **Epoch store updated first.** Before calling the cache, `commit_certificate` records the
   digest in the epoch store's `executed_in_epoch` set, and for non-digest transaction keys
   (randomness, settlement) the key→digest map (AUTH:1850-1858). The comment there gives the
   ordering rule: insert into the epoch store first, and on lookup check the perpetual store
   first.

### 2.3 Steps, in order (WBC:936-1055)

1. Assert `!transaction_executed_in_last_epoch(tx, epoch)`.
2. **Tombstones first.** For each `deleted` key, `write_object_entry(id, v, Deleted)`; then for each
   `wrapped` key, `write_object_entry(id, v, Wrapped)` (WBC:959-969).
   *Why:* a deleted object may be a child (dynamic field). If the parent were written first, a
   reader at the parent's new version could look up the child by version bound and see the
   child's previous live version instead of the tombstone. That would be an execution fork
   (WBC:959-962).
3. **Markers.** For each `(FullObjectKey, MarkerValue)`, `write_marker_value(epoch, key, value)`
   (WBC:971-974, 663-690), which inserts into `dirty.markers[(epoch, id)]` at `key.version()` and,
   for `ConsensusStreamEnded`, notifies `object_notify_read` for that `VersionedObject` key.
4. **Children, then parents.** First every written object with `is_child_object()` (owner is
   `ObjectOwner`), then all the others (WBC:976-992). Packages are inserted into `packages` when
   written.
   *Why:* the child-object consistency rule (EC:595-602): a reader that observes a parent at
   version V must observe every child at its correct version <= V. Children are read with a
   version bound equal to the parent's version, so they must be visible no later than the parent.
5. `pending_transaction_writes.insert(tx, outputs)` (WBC:1005-1008).
6. `transaction_effects.insert(effects_digest, effects)` **before** `executed_effects_digests`
   (WBC:1010-1015). *Why:* there must never be a dangling `executed_effects_digests` entry: a reader
   who sees "executed with digest D" must be able to fetch D.
7. `transaction_events.insert(tx, events)`, even if empty (WBC:1017-1023).
8. `unchanged_loaded_runtime_objects.insert(tx, ...)` (WBC:1025-1029).
9. `executed_effects_digests.insert(tx, effects_digest)` (WBC:1031-1034). This is the
   linearization point for "the transaction is executed".
10. `executed_effects_digests_notify_read.notify(tx, effects_digest)` (WBC:1036-1037). It must come
    after step 9, so that a waiter that registers and then reads finds either the entry or the
    notification (4.9).
11. `total_transaction_inserts += 1`; compute `pending = inserts - commits` and update backpressure
    (WBC:1043-1054, 8.3).

Note: because `pending_transaction_writes` (step 5) is inserted before the effects (step 6),
`multi_get_transaction_blocks` can return the transaction before its effects are visible. That is
harmless: readers that need "executed" use step 9. The historical ordering bug is `5935eabf0e`
(section 11).

### 2.4 `write_object_entry(id, version, entry)` (WBC:605-661)

1. Take the dirty map entry for `id`: `dirty.objects.entry(id).or_default()`. This holds the
   DashMap shard **write lock** until the function returns.
2. While holding it, insert `Object(version, entry)` into `object_by_id_cache` with
   `Ticket::Write`, ignoring failure (WBC:635-643).
3. Insert `(version, entry)` into the dirty version map. This calls `fatal!` if the version is not
   greater than the current highest.
4. Notify `object_notify_read` (still under the entry lock): for a package,
   `InputKey::Package{id}`; for a non-child object, `InputKey::VersionedObject{id: full_id,
   version}`. Child objects and tombstones do not notify.

**Why the dirty entry lock is held across the `object_by_id_cache` insert** (WBC:614-632): without
it, TX1 can put (O,1) in the dirty set and stall; TX2, which reads (O,1), is scheduled as soon as
its inputs exist (the scheduler does not wait for TX1 to finish), executes, and writes (O,2) to
both dirty and `object_by_id_cache`; then TX1 resumes and writes (O,1) to `object_by_id_cache`.
The latest-object cache would then be stuck at an old version. The `IsNewer` check inside
`MonotonicCache` also rejects the stale write (CT:309-313, `debug_fatal!`), but the lock makes it
impossible by construction. Fixed in `5c6968bbd4` and `6226f54ad8` (section 11).

### 2.5 Visibility

- Everything written is visible to every reader of the cache as soon as the individual
  insertion completes. There is **no** per-transaction atomicity: a reader can see some of a
  transaction's objects but not others. Safety comes from the ordering rules (tombstones, then
  markers, then children, then parents, then effects, then executed digest) together with the
  scheduler, which only reads inputs at their exact assigned versions.
- Dirty data is visible to: execution (input loading, child reads, receiving reads);
  `validate_owned_object_versions` (signing and voting); RPC reads; `notify_read_*` waiters; and the
  checkpoint builder (via `notify_read_executed_effects`).
- Dirty data is lost on crash and discarded at epoch end (section 6).

### 2.6 Notifications fired by writes

| Event | Registry | Key | Value |
|---|---|---|---|
| non-child live object written | `object_notify_read` | `VersionedObject{full_id, version}` | `()` |
| package written | `object_notify_read` | `Package{id}` | `()` |
| `ConsensusStreamEnded` marker | `object_notify_read` | `VersionedObject{marker FullObjectID, version}` | `()` |
| transaction executed | `executed_effects_digests_notify_read` | tx digest | effects digest |

Notifications are **not** fired on commit, by state-sync inserts, or by genesis inserts.

---

## 3. Commit path

### 3.1 Who commits, and when

Only the checkpoint executor commits transaction outputs, once per checkpoint, after every
transaction in the checkpoint has executed locally (CE:357-530):

1. Every transaction in checkpoint `seq` has executed: either they were already executed
   (validator path, `verify_locally_built_checkpoint`), or the executor enqueued them and waited
   on `notify_read_executed_effects_digests` (CE:632-665).
2. `batch = cache_commit.build_db_batch(epoch, &tx_digests)` (CE:415-418), where `tx_digests` is in
   checkpoint order.
3. `set_highest_committed_checkpoint_in_batch(&mut batch, seq)` stages the
   `highest_committed_checkpoint` watermark in the **same** batch (CE:420-427, AST:144-160,
   645-656). Added in `94bfdb24ad`.
4. Accumulator unsettled-withdrawal GC: `commit_accumulator_versions(...)` (CE:431-449).
5. In `spawn_blocking`: `cache_commit.commit_transaction_outputs(epoch, batch, &tx_digests)`
   (CE:451-466).
6. `epoch_store.handle_finalized_checkpoint(...)` deletes `signed_effects_digests` for these
   transactions, advances the consensus quarantine watermark (which may flush consensus outputs to
   the epoch db), and prunes `executed_in_epoch` (CE:470-472;
   `authority_per_epoch_store.rs:2057-2089`).
7. Randomness reporting, subscription publishing, running state-hash accumulation, end-of-epoch
   bookkeeping.
8. `bump_highest_executed_checkpoint(checkpoint)` (CE:523, 1067-1128), which asserts
   `prev + 1 == seq`.

**Ordering across checkpoints.** `BuildDbBatch` and `CommitTransactionOutputs` are pipeline
stages. `PipelineStages` uses a `SequenceWatch` per stage, so stage S for checkpoint N begins only
after stage S for checkpoint N-1 has finished (`checkpoint_executor/utils.rs:340-463`). So,
although checkpoints execute in parallel up to `checkpoint_execution_max_concurrency`, **batch
building and commits happen strictly in checkpoint sequence order**, and so does the
highest-executed bump. This is required, because `pop_oldest` asserts causal order per object
(3.4).

**Ordering inside a checkpoint.** Transactions in a checkpoint are in causal order (checkpoint
contents are a topological order of the effects DAG). `build_db_batch` keeps that order
(WBC:1060-1076). The per-object flush (`flush_objects_from_dirty_to_cached`) runs sequentially in
that order, because several transactions in the same checkpoint can write consecutive versions of
the same shared object (WBC:1118-1124).

### 3.2 `build_db_batch(epoch, digests)` (WBC:1057-1083)

For each digest, in order:

- if `pending_transaction_writes[tx]` exists, clone its `Arc<TransactionOutputs>` into
  `all_outputs`;
- otherwise log a warning and **skip it**. This is expected when the process crashes after the
  commit batch was written but before `highest_executed_checkpoint` was bumped: on restart the
  same checkpoint is processed again, its transactions are already in the db (so they are not
  re-executed and not dirty), and they must simply be skipped (WBC:1067-1073).

Then `store.build_db_batch(epoch, &all_outputs)` (AS:696-720) appends, for each transaction in
order (`write_one_transaction_outputs`, AS:722-810):

1. `effects[effects_digest] = effects` and then `executed_effects[tx] = effects_digest`. The
   effects are inserted first because, under Tidehunter, they carry the epoch information that
   the relocation filters need for pruning (AS:742-752).
2. `transactions[tx] = transaction`.
3. `executed_transaction_digests[(epoch, tx)] = ()`.
4. `object_per_epoch_marker_table_v2[(epoch, key)] = marker` for every marker.
5. `objects[(id, v)] = Deleted` for each deleted key, and `Wrapped` for each wrapped key.
6. `objects[(id, v)] = StoreObject(obj)` for each written object.
7. `events_2[tx] = events`, only if `effects.events_digest().is_some()`.
8. `unchanged_loaded_runtime_objects[tx] = list`, only if non-empty.

The batch holds **no deletes** of older object versions (the pruner removes those later) and no
lock-table changes (locks are no longer kept here). It returns `(Vec<Arc<TransactionOutputs>>,
DBBatch)`.

Batch building is split from batch writing (commit `84e622208e`) so that the executor can add the
watermark to the batch (3.1 step 3) and pipeline the CPU work separately from the write.

### 3.3 `commit_transaction_outputs(epoch, (all_outputs, batch), digests)` (WBC:1087-1140)

1. Failpoint `writeback-cache-commit`.
2. **Write the db batch first**: `db_batch.write()`, which panics on error (WBC:1098-1101).
   *Why:* data must be durable before it leaves the dirty set. If it were removed from the dirty
   set first, a concurrent reader could miss in dirty, miss in the cache (an eviction is always
   possible), and then miss in the db: the value would briefly disappear, possibly producing a
   wrong negative answer (WBC:1098-1100).
3. **Parallel phase: transaction metadata** (WBC:1105-1116). For each output (rayon, min chunk
   16):
   - `assert!(pending_transaction_writes.remove(tx).is_some())`;
   - `flush_tx_metadata_from_dirty_to_cached` (WBC:1168-1237): first insert into
     `cached.transactions`, `cached.transaction_effects`, `cached.executed_effects_digests` and
     `cached.transaction_events` with `Ticket::Write`; **then** remove from
     `dirty.transaction_effects`, `dirty.transaction_events`,
     `dirty.unchanged_loaded_runtime_objects` and `dirty.executed_effects_digests`, each with
     `.expect("... must exist")`. Inserting into the cache before removing from dirty avoids
     needless misses. Correctness does not depend on it, since the db already has the data.
   These can run in parallel because the keys (tx digest, effects digest) are unique per
   transaction (commit `a9e9a9ee9c`).
4. **Sequential phase: objects and markers**, in `all_outputs` order (WBC:1118-1124).
   `flush_objects_from_dirty_to_cached(epoch, outputs)` (WBC:1242-1290) calls
   `move_version_from_dirty_to_cache` for each marker (key `(epoch, id)`), each written object,
   each deleted key and each wrapped key.
5. `total_transaction_commits += n`; recompute backpressure (WBC:1126-1139).

Note: the `pending_transaction_writes` entry is removed (step 3) before the object versions move
(step 4). In that window `multi_get_transaction_blocks` reads the transaction from the cached set
or the db (it is in both), and `take_accumulator_events` returns `None`. Both are safe.

### 3.4 `move_version_from_dirty_to_cache(dirty, cache, key, version, value)` (WBC:1294-1329)

1. Lock the dirty entry: `dirty.entry(key)`, which takes the DashMap shard write lock.
2. Lock the cache entry: `cache.entry(key).or_default()` then `.lock()` on its mutex.
3. `cache_map.insert(version, value)`, then `cache_map.truncate_to(3)`.
4. The dirty entry must be occupied (`panic!("dirty map must exist")`). Then
   `removed = pop_oldest(version)` and assert `removed == Some(value)` ("dirty version must
   exist").
5. If the dirty map is now empty, remove the key.

*Why both locks are held:* readers take the same two locks (`with_locked_cache_entries`,
WBC:695-716), so a reader sees either the "before" state (version in dirty) or the "after" state
(version in the cache); it never sees the value missing from both.

*Why the cached insert can be recreated by `or_default`:* if the moka entry for this object was
evicted, the new cached map starts with just this version. That is still a valid contiguous
suffix, because every older version is in the db and every newer version is in dirty.

*Why `pop_oldest` asserts:* committing V+1 while V is still dirty would leave V dirty but older
than a cached version. That breaks the "cached versions < dirty versions" ordering, and with it
negative hits (test `test_out_of_order_commit`, TESTS:661-674).

### 3.5 Interaction with cached reads

- `object_by_id_cache` is **not** touched by commit. It already holds the latest version, since it
  was written when the version was dirty.
- `packages` is not touched by commit.
- Point caches receive `Ticket::Write` inserts on commit, which expire any concurrent read
  ticket. A reader that saw `None` in the db just before the batch was written cannot then cache
  a negative entry over the committed value (5.2).

### 3.6 Other writers to the db that bypass the dirty set

- **`persist_transaction(tx)`** (WBC:1539-1541, AS:814-822): writes only `transactions[tx]`,
  immediately. It is used for `RandomnessStateUpdate` transactions
  (`randomness_round_receiver.rs:297`), which cannot be reconstructed by consensus replay after a
  crash (EC:180-183). It does **not** touch `cached.transactions`. A negative entry for that digest
  is therefore possible in theory, but in practice no one reads the transaction before it is
  executed.
- **StateSyncAPI** `insert_transaction_and_effects` and `multi_insert_transaction_and_effects`
  (WBC:2525-2582): write the transaction and effects (certified data from state sync) to the db
  first, then insert them into `cached.transactions` and `cached.transaction_effects` with
  `Ticket::Write`, which invalidates any concurrent negative caching. They do **not** write
  `executed_effects`, so the transaction is not marked executed. The comment (WBC:2520-2524)
  states the minimum requirement: at least invalidate, because the items could be negatively
  cached. Test: `test_transaction_cache_race` (TESTS:1103-1164). Fix: `11e5ec9bbe`.
- **Genesis** `bulk_insert_genesis_objects` and `insert_genesis_object` (WBC:1399-1413): write to
  the db, then invalidate `object_cache` and `object_by_id_cache` for those ids (the order differs
  between the two functions; both are only called when no execution is running).
- **Checkpoint executor and epoch-store tables** (`executed_transactions_to_checkpoint`, the
  deprecated per-tx checkpoint table, the watermark) are separate tables, not cached here.

---

## 4. Read paths

General rules:

- Lookup order is always **dirty → committed cache → db**, and for objects the latest-object
  cache comes first.
- Every multi-get uses `do_fallback_lookup` (`fallback_fetch.rs:17-60`). Per key the cache answers
  `Hit(v)`, `NegativeHit` (the result is the default `None`/`false`), or `Miss` (the key goes to one
  batched db multi-get).
- Read paths **never** write to the dirty set. They write to the committed caches only for point
  negatives and for `object_by_id_cache` and `packages`, always under a ticket (section 5).
- DB errors panic (`expect("db error")`). The read API returns `Option`, not `Result`
  (commit `9289d3dc46`).

### 4.1 `get_object_by_key(id, version)` (WBC:1616-1624)

`get_object_entry_by_key_cache_only` (WBC:720-747), under `with_locked_cache_entries` (dirty
shard lock + cached mutex):

1. Dirty map, if present: an exact `get(version)` → `Hit`. Else, if `least_version < version` →
   `NegativeHit`. Else fall through.
2. Cached map, if present: the same rules.
3. → `Miss`.

A `Hit` on a tombstone maps to `NegativeHit`. A `Miss` goes to `store.get_object_by_key`, an
exact db get, which returns `None` for tombstones.

Negative-hit reasoning: if `version > least` in a contiguous suffix and it is not present, it
lies either in a gap between two present versions (so it never existed) or above the highest
(so it does not exist yet). Example (TESTS:715-777): versions 1, 3, 5; a query for 2 or 4 is
negative, 6 is negative.

`multi_get_objects_by_key`, `object_exists_by_key` and `multi_object_exists_by_key` follow the
same logic (WBC:1626-1667).

### 4.2 `get_object(id)`: latest live object (WBC:890-922, 1612-1614)

1. **Take a read ticket first**: `ticket = object_by_id_cache.get_ticket_for_read(id)`.
2. `get_object_entry_by_id_cache_only` (WBC:764-842):
   a. `object_by_id_cache.get(id)`: `Object(v, entry)` → `Hit((v, entry))`; `NonExistent` →
      `NegativeHit`.
   b. Otherwise, under `with_locked_cache_entries`: the dirty highest → `Hit`; else the cached
      highest → `Hit`; else `Miss`. An empty version map that is still present would panic
      ("empty CachedVersionMap should have been removed", WBC:492), since empty maps are always
      removed.
3. `Hit` on a tombstone → `None`; `NegativeHit` → `None`.
4. `Miss` → `store.get_latest_object_or_tombstone(id)`, a reverse range scan on `objects`. Then:
   - found (object or tombstone) → `cache_latest_object_by_id(id, Object(v, entry), ticket)`;
   - not found → `cache_object_not_found(id, ticket)`, which inserts `NonExistent`.
   The insert silently fails if the ticket expired, and the `expired_tickets` metric is
   incremented (WBC:1331-1349).

`get_latest_object_ref_or_tombstone` and `get_latest_object_or_tombstone` (WBC:1669-1717) use the
same cache-only step but **do not** populate the cache on a db miss. A tombstone is returned as an
`ObjectRef` with digest `OBJECT_DIGEST_DELETED` or `OBJECT_DIGEST_WRAPPED`.

### 4.3 `find_object_lt_or_eq_version(id, bound)` (WBC:1739-1903)

This is used for child-object (dynamic field) reads (`read_child_object`, EC:727-749), MVCC
accumulator reads, and the indexer. It returns the highest version <= `bound`, or `None` if that
version is a tombstone or does not exist.

1. `object_by_id_cache.get(id)` (the `Arc` is kept, and the lock released):
   - `Object(v, entry)` with `v <= bound`: return the object, or `None` for a tombstone. This is the
     fast path for read-hot, write-cold objects.
   - `NonExistent` → `None`.
   - `Object(v, _)` with `v > bound` → fall through.
2. Under `with_locked_cache_entries(dirty, cached)`:
   a. Dirty map: the first entry from `all_versions_lt_or_eq_descending(bound)`. If found: live →
      return it; tombstone → `None`.
   b. Cached map: the same.
   c. (The metric labels for 2a and 2b are swapped in code, WBC:1812-1813. This is cosmetic.)
   d. Compute `latest`: if a dirty map exists, its highest (**panics** if empty); else
      `store.get_latest_object_or_tombstone(id)`. This db read happens **while holding the dirty
      shard lock and the cached mutex** (TODO at WBC:1842).
   e. If `latest` exists: **always** cache it into `object_by_id_cache`, with a ticket taken at
      that moment. That is safe because the dirty lock is held, so no writer can interleave
      (WBC:1856-1865). It warms the cache for read-hot children that are never written. Then:
      if `latest.version <= bound`, return it (or `None` for a tombstone); else do a db range scan
      `store.find_object_lt_or_eq_version(id, bound)`. Dirty and cached were already scanned and
      had nothing <= bound, so the answer, if any, is only in the db.
   f. If `latest` does not exist (nothing in dirty or db): if step 1 found an entry, assert it is
      **not alive** (`assert!(!is_alive())`). It can be a tombstone whose db rows were pruned.
      Otherwise assert that the cached highest is absent or a tombstone, and cache `NonExistent`.
      Return `None`.

**Why the child read is correct with respect to concurrent writes:** children are written before
parents (2.3 step 4), tombstones before everything else (2.3 step 2), and the version bound is the
parent's version. Dirty versions are a contiguous suffix, so if any dirty version is <= bound it
is the right answer. If none is, every dirty version is > bound and the answer is in the cached
set or the db. Test: `test_concurrent_readers` (TESTS:903-978), where a reader that sees the new
parent must see the deleted child as `None`.

### 4.4 Markers (WBC:844-888, 1913-1943)

- `get_marker_value(key, epoch)`: version-exact. The same `check_cache_entry_by_version` logic on
  dirty and then cached markers, including negative hits; on a `Miss`, a db point get on
  `EpochMarkerKey(epoch, key)`.
- `get_latest_marker(full_id, epoch)`: dirty highest → cached highest → db reverse scan bounded to
  that `(epoch, full_id)`. A `NegativeHit` here is impossible and panics.
- Helpers in EC: `get_last_consensus_stream_end_info`, `get_consensus_stream_end_tx_digest`,
  `have_received_object_at_version`, `fastpath_stream_ended_at_version_or_after` (EC:365-413).

Note that the committed marker cache keeps at most 3 versions, and a `ConsensusStreamEnded`
stream is smeared across many versions. A latest-marker query after eviction falls back to a
correct db scan, because markers are flushed to the db in the same batch as everything else.

### 4.5 Transactions, effects, executed digests, events (WBC:2019-2329)

The same pattern for each:

1. **Take a read ticket for each key before any lookup** (e.g. WBC:2024-2027).
2. Dirty map hit → `Hit(Some)`.
3. Committed `MonotonicCache`: `Some(Some(v))` → `Hit`; `Some(None)` → `NegativeHit`; absent →
   `Miss`.
4. DB multi-get for the misses. For each result that is `None`, insert a negative entry with the
   **read ticket**, so that a concurrent writer that wrote with `Ticket::Write` wins
   (WBC:2073-2077, 2136-2143, 2197-2204, 2318-2325). Positive db results are **not** cached on the
   read path. Only commit and state sync insert positive entries.

Events: an empty events struct maps to `None` (WBC:2264-2270). Events db reads go directly to
`self.store` without the db-request metric (WBC:2314-2317).

Derived APIs in EC: `multi_get_executed_effects` = executed digests → effects (EC:460-484);
`is_tx_already_executed`; `get_executed_effects`.

**Hazard:** an executed digest can be visible in dirty while the effects are not (or the reverse)
only during the write. The ordering in 2.3 (effects before digest) guarantees that the digest
implies the effects. At commit time the effects and the digest are both in the db before either
is removed from dirty.

### 4.6 `get_unchanged_loaded_runtime_objects` (WBC:2331-2362)

Dirty → db. No cache layer and no negative caching.

### 4.7 `transaction_executed_in_last_epoch(tx, current_epoch)` (WBC:2210-2246)

`current_epoch == 0` → `false`. Key `(current_epoch - 1, tx)`. A read ticket is taken first; a
cache hit returns `is_some()`. Otherwise a db point lookup on `executed_transaction_digests`, and
both answers are cached with the read ticket. Nothing in the current epoch ever writes key
`(E-1, tx)`, so the ticket is effectively always valid.

### 4.8 `get_package_object(id)` (WBC:1549-1603)

1. `packages.get(id)` hit → return it. In debug builds, assert that the digest equals the canonical
   package (dirty highest, else db).
2. Else `get_object_impl("package", id)`: the full latest-object path, including dirty. If the
   result is a package, insert it into `packages` and return it; if it is a Move object, return
   `UserInputError::MoveObjectAsPackage`; if it is absent, return `Ok(None)`.

### 4.9 `notify_read_executed_effects_digests(task, digests)` (WBC:2248-2258; NotifyRead in `crates/mysten-common/src/sync/notify_read.rs:241-260`)

The semantics, which must be preserved exactly:

1. **Register** a waiter for every key.
2. **Then** fetch with `multi_get_executed_effects_digests` (dirty, then cache, then db).
3. Return the found values immediately, and drop their registrations. Await the rest.

Register-then-fetch, combined with write-then-notify on the writer side (2.3 steps 9-10), means no
wakeup can be lost. It resolves on **dirty** (uncommitted) execution.

`notify_read_executed_effects` and `..._may_fail` (EC:552-589) then fetch effects by digest. The
first panics if any are missing; the second returns `TransactionEffectsNotFound` (the effects may
have been pruned). **Warning** (EC:540-546): if you wait on a transaction that might be reverted at
an epoch boundary, the wait must be cancelled at reconfiguration (`within_alive_epoch`). Otherwise
it can hang forever, or observe effects that are about to be discarded. Fix: `5aea124ff8`.

### 4.10 Input availability: `multi_input_objects_available`, `notify_read_input_objects`

**Cache-only fast path** `multi_input_objects_available_cache_only(keys)` (WBC:1719-1737): a
cancelled key → available. `VersionedObject` → available iff `get_object_by_key_cache_only` is a
`Hit`. Tombstone hits are mapped to `NegativeHit` there, so only live objects in memory count. `Package` → `packages.contains_key`. It never
touches the db. The scheduler calls it first (`execution_scheduler_impl.rs:257-277`).

**Full check** `multi_input_objects_available(keys, receiving, epoch)` (EC:261-327):

- cancelled → available;
- `VersionedObject` exists at exactly that version (dirty, cache or db) → available;
- else, if it is a receiving key: available iff the latest live version is >= the requested
  version, **or** a `FastpathStreamEnded` marker exists at version >= requested. The transaction is
  let through so that it fails at execution. This is needed because the requested version may
  already have been pruned (EC:294-309);
- else: available iff a `ConsensusStreamEnded` marker exists at exactly that version (EC:310-317);
- `Package` → `get_package_object(id)` is `Ok(Some)`. An error (not a package) → unavailable.

**`notify_read_input_objects(keys, receiving, epoch)`** (WBC:1997-2016): `object_notify_read.read`
with `fetch = multi_input_objects_available`; register then check, as in 4.9. It wakes on
`VersionedObject` notifications (non-child object writes and `ConsensusStreamEnded` markers) and on
`Package` notifications. The scheduler races it against "the transaction was already executed"
(`execution_scheduler_impl.rs:304-325`).

Receiving keys: a waiter for receiving object O at version v is woken only by a write of (O, v)
itself. If O is written at a higher version, or a marker is written instead, the waiter is not
notified for that key. Receiving objects are fastpath-owned and their versions come from the
sender's view, so in practice the scheduler checks this case on registration (immediate
availability, NRTESTS:279-308). A re-implementation should either wake receiving waiters on any
write to the id or keep this registration-time check.

### 4.11 `load_implicitly_read_system_object(id, ConsensusObjectVersion)` (WBC:500-549)

This is for system objects that execution reads implicitly at an assigned version (e.g. the
accumulator root); the id must be in `IMPLICITLY_READ_SYSTEM_OBJECTS`. Steps:

1. Try `get_object_by_key(id, version)`.
2. Else, **block the OS thread** with `object_notify_read.read_one_blocking` on
   `VersionedObject{Consensus(id, isv), version}`. The fetch condition is "the latest version is >=
   version". It deliberately uses >= rather than ==: in a dry run the pinned version may have been
   superseded and pruned, and the call must not wait forever.
3. Return `get_object_by_key(id, version)`, which can be `None` only in that superseded case
   (NRTESTS:381-406).

It must be called from a blocking thread. The execution permit is released while waiting
(NRTESTS:310-379). Added in `b8a3bbc7ad`.

### 4.12 Accumulator (address balance) reads (WBC:1425-1510)

- `get_latest_account_amount(id)`: `get_object(id)` → `u128` value, or 0 if absent.
- `get_account_amount_at_version(id, v)`: `find_object_lt_or_eq_version(id, v)` → value, or 0.
- `get_consistent_latest_account_amount_and_version(id)` (WBC:1438-1498):
  `pre = get_object(ACCUMULATOR_ROOT).version`; loop { `value = amount_at_version(id, pre)`;
  `post = root version`; if `pre == post`, return `(value, pre)`; else `pre = post` }. *Why:*
  settlement is not atomic. Settlement transactions write account objects at V+1 (possibly
  tombstones) before the barrier transaction bumps the root from V to V+1. Reading the latest
  account could pair a post-settlement balance with a pre-settlement root. Capping the read at
  the root version gives the account value as of that root. The pre==post check guarantees that
  no root advance happened during the read, and therefore that pruning (which is tied to root
  advancement) cannot have removed the version that was read. Regression test:
  TESTS:1166-1230. Fix: `0192aba594`; refinements `8c1a5dbc40`, `79585efd9e`, `147abcdf7f`.

### 4.13 `GlobalStateHashStore` reads (WBC:2389-2518)

- `get_object_ref_prior_to_key_deprecated(id, v)`: collects candidates from the dirty map
  (`get_prior_to`), the cached map and the db, and returns the highest. This is for old protocol
  versions only.
- `iter_live_object_set`: **asserts the dirty set is empty**, then iterates the db. It is legal
  only at an epoch boundary, after clear (WBC:2465-2477).
- `iter_cached_live_object_set_for_testing`: merges the db and dirty highest versions while holding
  a dirty iterator. Test only.

### 4.14 How negative caching avoids stale data: summary

1. Version-keyed data (objects, markers): **no explicit negative entries**. Negativity is derived
   from the contiguity of the in-memory queues, which are maintained only by write and commit,
   never by reads.
2. Latest-object data: explicit `NonExistent` and tombstone entries in a `MonotonicCache`. They
   are inserted from db reads only with a read ticket taken **before** the lookup began. Every
   dirty write bumps the generation (`Ticket::Write`), which invalidates in-flight read tickets,
   and also overwrites the entry. `IsNewer` prevents any older value from replacing a newer one.
3. Point data (tx, effects, executed digests, events): negative entries are inserted only with a
   read ticket taken before the dirty/cache/db lookup. Commits and state-sync writes use
   `Ticket::Write`. Because these collections are insert-only, `Some` is always newer than
   `None`, and two `Some` values must be equal (`debug_assert_eq!`, WBC:301-320).
4. Discarding dirty data (end of epoch) explicitly invalidates `object_by_id_cache`, the cached
   object map and `packages` for every affected id (6.2).

---

## 5. Concurrency and consistency

### 5.1 `MonotonicCache<K, V>` and tickets (CT:158-335)

State: a moka cache `K → Arc<Mutex<V>>`, and `key_generation: Vec<AtomicU64>` of size
`16 × 1024`, indexed by `hash(K) % 16384` (CT:172, 185-190, 210-215).

- `get_ticket_for_read(k)` → `Ticket::Read(gen[k].load(Acquire))`. **The caller must take the
  ticket BEFORE checking dirty or the db** (CT:217-226).
- `insert(k, v, ticket)` (CT:233-317):
  1. If `ticket == Write`: `gen[k].fetch_add(1, Release)` **first**, which invalidates every
     outstanding read ticket for every key in that bucket.
  2. `entry(k).or_optionally_insert_with(|| check_ticket().ok().map(|_| Arc::new(Mutex::new(v))))`.
     If the entry was absent and the ticket is valid, insert it. If the entry was absent and the
     ticket is invalid, insert nothing and return `Err`.
  3. If the entry was already present: lock it, `check_ticket()?` **under the entry lock**, then,
     if `entry.is_newer_than(v)`, `debug_fatal!`; else overwrite.
- The comment at CT:255-301 enumerates all reader/writer interleavings and shows that the newest
  value always wins and a stale reader never overwrites a fresh writer.
- `Ticket::Write` inserts may also return `Err` (a reader won the `or_optionally_insert_with` race
  but then failed its ticket check, so nothing was inserted). Callers ignore that error
  (WBC:641-643); the cache simply has no entry, which is safe.
- Collisions in the generation array cause only false ticket expiries, which are lost cache
  fills, never incorrect data (CT:185-189).
- `invalidate(k)` removes the entry **without** bumping the generation. A concurrent reader with a
  pre-invalidation ticket can therefore re-insert a value that was current when it read. That is
  fine for committed data, but see 6.2 for the end-of-epoch case.

Regression test: `latest_object_cache_race_test` (TESTS:980-1101). A writer thread, a stale reader
that takes its ticket before reading, an invalidator and a monotonicity checker run together. If
the ticket is taken after the read, the test fails.

### 5.2 Locks and their order

| Lock | Holder |
|---|---|
| `dirty.objects` / `dirty.markers` DashMap shard (write) via `.entry()` | writers (`write_object_entry`, `write_marker_value`), commit (`move_version_from_dirty_to_cache`), readers (`with_locked_cache_entries`) |
| cached version-map `Mutex` (inside the moka value) | commit, readers |
| `MonotonicCache` entry `Mutex` | `object_by_id_cache` insert/get, point caches |
| `execution_lock: RwLock<EpochId>` (tokio) | execution (read, `try_read`), validation (read), reconfiguration (write) |
| per-tx `mutex_table` lock (`acquire_tx_guard`) | one execution per digest |

**Order:** dirty shard → cached version-map mutex → `object_by_id_cache` entry mutex. Writers hold
the dirty shard while inserting into `object_by_id_cache`. `find_object_lt_or_eq_version` holds
the dirty shard and the cached mutex while inserting into `object_by_id_cache` and while
reading the db. `get_object_entry_by_id_cache_only` takes the `object_by_id_cache` lock and
releases it before taking the dirty and cached locks. No code takes a dirty or cached lock while
holding an `object_by_id_cache` entry lock. Keep this order.

Readers use `dirty.entry(key)`, which is a **write** lock on the shard, not `get`. That is what
makes the "move from dirty to cache" atomic to them (WBC:695-716). A read-lock design would also
work, as long as commit holds both locks while it moves the version.

### 5.3 Atomicity requirements

1. **Dirty → cached move:** atomic with respect to readers of that key (3.4).
2. **DB write → dirty removal:** the db write happens-before the removal of any dirty entry from
   that batch (3.3 step 2).
3. **Dirty object insert ↔ `object_by_id_cache` update:** under the same dirty entry lock (2.4).
4. **Effects insert → executed digest insert → notify:** in that order (2.3 steps 6, 9, 10).
5. **Register → fetch** for every `notify_read` (4.9).
6. **Commit batch:** one RocksDB write batch per checkpoint holds every output of every
   transaction in the checkpoint plus the `highest_committed_checkpoint` watermark. It is atomic
   on disk (commit `722a87ab77` introduced one batch per checkpoint; `94bfdb24ad` added the
   watermark).
7. **Reconfiguration** happens under the execution write lock, so there are no concurrent writes or
   commits during clear (6.1). Reads (RPC) can still run concurrently.

### 5.4 No transaction-level isolation

Readers can observe a partially written transaction. This is safe only because of:

- write ordering (tombstones → markers → children → parents → effects → executed digest);
- the scheduler reading inputs only at exact assigned versions that exist (an input is not
  "available" until the object is written at that version, and a transaction's outputs are its
  inputs' successors);
- child reads bounded by the parent version.

A re-implementation that wants simpler reasoning can publish a transaction's outputs atomically
(e.g. under a sequence lock). It must preserve at least the ordering guarantees above.

---

## 6. Reconfiguration and epoch boundaries

### 6.1 Sequence (AUTH:3817-3909)

1. `execution_lock_for_reconfiguration().await`: the write lock waits until no transaction is
   executing. Any later execution attempt `try_read`s and fails with `EpochEnded`, or finds an epoch
   mismatch (AUTH:3791-3814, 1580-1594).
2. `cur_epoch_store.epoch_terminated()`: cancels every `within_alive_epoch` task, including
   `notify_read_executed_effects` waiters on transactions that may be reverted (4.9).
3. **`clear_state_end_of_epoch(&execution_lock)`** (WBC:1355-1397):
   a. For every remaining `pending_transaction_writes` entry: `debug_fatal!` if the transaction has
      shared inputs. Only single-writer (owned-input-only) transactions can be left uncommitted at
      epoch end, because every consensus-sequenced transaction of the epoch must have been in a
      checkpoint of that epoch.
   b. For each such transaction: for every written id, invalidate `packages` (if it is a package),
      `object_by_id_cache` and `cached.object_cache`; for every deleted or wrapped id, invalidate
      `object_by_id_cache` and `cached.object_cache`.
   c. `dirty.clear()`: every dirty map and both counters are reset.
   d. `object_locks.clear()`: a no-op today.
   e. `store.clear_object_per_epoch_marker_table(guard)`: deletes every row of both marker tables
      (`schedule_delete_all`, or a Tidehunter range drop). It requires the write guard as proof
      that this is reconfiguration (AS:183-208).
4. `check_system_consistency` (`iter_live_object_set`, which asserts the dirty set is empty),
   `maybe_reaccumulate_state_hash`, `set_epoch_start_configuration`, an optional db checkpoint,
   `reconfigure_cache` (a no-op), reopen the epoch db, reconfigure the scheduler.
5. `*execution_lock = new_epoch` and release it.

**Why reverting is just "drop dirty":** with the writeback cache the db holds only committed,
certified state, so reverting uncommitted execution means forgetting the in-memory dirty data and
invalidating any cache that might hold it. The old explicit `revert_state_update` (per-transaction
db reverts) was deleted in `bca8a306df`.

**Why the committed caches are not cleared:** they hold only committed, final data. `marker_cache`
and `transaction_executed_in_last_epoch` are keyed by epoch.

**What is epoch-scoped:**

- markers: keyed by epoch in memory and in the db, and the db table is cleared at reconfiguration;
- `executed_transaction_digests`: keyed by epoch and kept (used for the E-1 check), pruned later;
- the dirty set: empty at the start of every epoch;
- outside the cache: the epoch store (`executed_in_epoch`, `signed_effects_digests`, owned-object
  locks, the consensus quarantine, `executed_transactions_to_checkpoint`).

### 6.2 Hazard: concurrent readers during clear

`clear_state_end_of_epoch` invalidates the caches (3b) **before** it clears dirty (3c). An RPC
reader running `find_object_lt_or_eq_version` between those two steps could take the dirty lock,
see an uncommitted version, and re-insert it into `object_by_id_cache` with a fresh ticket.
`invalidate` does not bump generations, and `dirty.clear()` does not either. A stale, reverted
latest version would then survive the clear. This interleaving has not been observed. A
re-implementation should make it impossible, for example by:

- clearing dirty **first** (under all shard locks) and invalidating afterwards; or
- bumping the read-ticket generations for the affected keys during the clear; or
- excluding readers during the clear.

### 6.3 Startup

A new `WritebackCache` starts empty (WBC:551-577). There is no warm-up; the db is the source of
truth. `build_execution_cache` (EC:114-131) is called once per process. The cache object is reused
across epochs; only `clear_state_end_of_epoch` resets it.

---

## 7. Crash and recovery

### 7.1 What is on disk at any instant

- **The perpetual db** contains, for each checkpoint `c <= highest_committed_checkpoint`, the full
  outputs of every transaction in `c`, with `highest_committed_checkpoint` written atomically with
  them. Nothing from a later checkpoint is on disk, except:
  - `persist_transaction` rows (randomness state updates: the transaction body only);
  - state-sync rows (the certified transaction and effects, but not executed digests or objects);
  - genesis objects, snapshot or formal-restore data.
- **The checkpoint store** has `highest_executed_checkpoint`, bumped in a separate write **after**
  the commit (CE:523). So `highest_executed <= highest_committed`, and they differ by at most one
  checkpoint after a crash in between (the crash failpoint is at CE:521).
- **The epoch db** has the consensus outputs that the quarantine has flushed: only those whose
  checkpoints have been executed **and** that end at a "checkpoint queue drained" point
  (CQ:594-677).

### 7.2 What a crash loses

All dirty data: every executed transaction not yet in a committed checkpoint, all dirty markers,
and all in-memory notifications. All committed caches (they are rebuilt lazily).

### 7.3 What is re-executed after restart

- **Checkpoint executor:** resumes at `highest_executed + 1` (CE:248-280). For each checkpoint it
  loads the transactions, checks `multi_get_executed_effects_digests` (now db only, since dirty
  is empty) and re-executes the missing ones with the **certified** expected effects digest; a
  mismatch is a fork panic (`assert_not_forked`). If the crash happened between the commit and the
  bump, the transactions of `highest_executed + 1` are already in the db: nothing is re-executed,
  `build_db_batch` skips the unknown digests (3.2), the watermark is rewritten, and the bump
  proceeds.
- If the last checkpoint of the epoch was executed and bumped but reconfiguration did not finish,
  `get_next_to_schedule` returns `None` (CE:259-267) and reconfiguration resumes.
- **Validators:** consensus commits after the last commit that the quarantine flushed are
  **replayed**. Shared-version assignments, deferred transactions, post-consensus owned locks and
  so on are reconstructed by the replay. Transactions in replayed commits are scheduled again; any
  that are already in the db (part of a committed checkpoint) are skipped by the
  executed-effects check.
- `RandomnessStateUpdate` transactions cannot be regenerated by consensus replay (they derive from
  DKG and randomness protocol state), so they are persisted at creation time (3.6).
- Locally built checkpoints are **not** re-executed at startup any more. That mechanism
  (`persist_transactions` plus `reexecute_local_checkpoints`, commit `befd9ffb64`) was replaced by
  consensus data quarantining (`e4db4a07c3`) and deleted in `ebfbc0c22b`. Today,
  `CheckpointService::spawn` deletes state hashes above `max(last persisted builder summary, last
  executed)` (fix `77bebabad1`). It then waits, with a 120 s timeout, until the builder has rebuilt
  every checkpoint that was built before the restart (`checkpoints/mod.rs:3007-3100`). The builder
  waits on executed effects, so this implies the replayed transactions were re-executed into the
  dirty set.
- Pending owned-object certificates whose effects this validator already signed are re-executed
  against the signed digest (`68113e14ae`).
- `signed_effects_digests` (epoch store) persists the effects digest this validator signed for
  each transaction, so a re-execution after a crash cannot equivocate. Execution is deterministic,
  so the digests should match anyway. These rows are deleted in `handle_finalized_checkpoint`.

### 7.4 Quarantine and the cache, compared

`ConsensusOutputQuarantine` (CQ:488-677) is the epoch-store analogue of the dirty set for consensus
outputs. Consensus commit outputs are kept in memory, in a `VecDeque` (FIFO), until the checkpoint
covering them has been executed (`update_highest_executed_checkpoint`, called from
`handle_finalized_checkpoint`). Then the builder summaries <= watermark, and the consensus outputs
up to the **last drain point** at or below the corresponding height, are flushed in one batch.
Reads check the quarantine first (ref-counted maps), then the db (CQ:757-826). Both designs hold
the same invariant: **anything that cannot be recomputed by replay must be persisted before the
input that would let you replay it is marked consumed; anything that can be recomputed stays in
memory until it is certified.**

### 7.5 Recovery invariants for a re-implementation

1. The commit batch and the "committed through checkpoint N" watermark must be one atomic write.
2. The "executed through N" watermark (the restart point) must be written after the commit batch.
   Commit must be idempotent for transactions already committed: skip them.
3. Restart must not read the dirty state (it is gone). Execution must be re-driven from certified
   checkpoints, plus replay of uncheckpointed consensus.
4. Data that cannot be regenerated must be persisted outside the dirty set.
5. Three stores are written non-atomically per checkpoint, in this order: the perpetual db
   (outputs plus `highest_committed_checkpoint`), the epoch db (quarantine release and
   signed-effects deletion), and the checkpoint store (`highest_executed`). Recovery must tolerate a
   crash between any two of these writes (`77bebabad1`, `4caaa72e3f`, `94bfdb24ad`).
6. Consensus output may be persisted, which ends its replay, only when every transaction it
   produced is in a checkpoint that has been executed **and committed through the cache**
   (`1ad56a6b81`, the drain-boundary rule, CQ:633-669). Otherwise a crash loses both the dirty
   outputs and the ability to regenerate them.
7. Signals that give up the ability to regenerate data (randomness "round included" notifications,
   pruning `signed_effects_digests`) fire only after the cache commit (`2de66e19de`;
   `authority_per_epoch_store.rs:2071`).

---

## 8. Memory bounds and eviction

### 8.1 Bounded (evictable) structures

All are moka `SegmentedCache` with 8 segments and TinyLFU by default. Capacities are entry counts
(`crates/sui-config/src/node.rs:715-861`, env overrides `SUI_*_CACHE_SIZE`). In tests, capacities
are randomized (`randomize_cache_capacity_in_tests`) to shake out eviction-dependent bugs.

| Cache | Default |
|---|---|
| `object_cache` | `max_cache_size` = 100,000 ids (≤3 versions each) |
| `marker_cache` | = object_cache_size |
| `object_by_id_cache` | = object_cache_size |
| `transactions` | `max_cache_size` = 100,000 |
| `executed_effects_digests` | = transaction_cache_size |
| `transaction_effects` | = executed_effect_cache_size |
| `transaction_events` | = transaction_cache_size |
| `transaction_executed_in_last_epoch` | = executed_effect_cache_size |
| `packages` | 1,000 |
| `_transaction_objects` | 1,000 (unused) |
| `MonotonicCache` generation array | 16,384 `AtomicU64` per cache (fixed) |
| `NotifyRead` | 255 mutex-sharded maps; entries removed when notified or when the future is dropped |

### 8.2 Never evicted

All of `dirty`: objects, markers, effects, events, executed digests, unchanged loaded objects,
and pending outputs. They are removed only by commit or by `clear_state_end_of_epoch`. **Eviction
of a dirty entry would be a correctness bug** (data loss, and broken negative hits).

The cached version maps are evicted as a whole (an entire object id). This is safe because the
remaining dirty queue is still a contiguous newest suffix (WBC:32-36).

### 8.3 Backpressure: the bound on dirty growth (WBC:1142-1163; `authority/backpressure.rs`)

`pending = total_inserts - total_commits` (approximate, relaxed atomics). After every write and
commit, `backpressure_manager.set_backpressure(pending > backpressure_threshold)`, with the
threshold defaulting to 100,000 (node.rs:839-850). Effects:

- the consensus handler calls `await_no_backpressure()` before processing more commits, which
  stops new work from entering execution;
- RPC transaction submission is rejected with `ValidatorOverloadedRetryAfter` when
  `pending > backpressure_threshold_for_rpc` (AUTH:1381-1389).

Backpressure is **suppressed** when `highest_certified_checkpoint <= highest_executed_checkpoint`
(backpressure.rs:16-20). If the executor has caught up with every certified checkpoint, nothing
more can be committed until consensus produces more checkpoints, so blocking consensus would
deadlock. The dirty set is therefore bounded only softly. Added in `769f14529c`.

---

## 9. Metrics and assertions that encode invariants

### 9.1 Metrics (`execution_cache/metrics.rs`)

- `execution_cache_requests`, `_hits`, `_negative_hits`, `_misses`, labelled `{request_type,
  level}`, where level ∈ {`uncommitted`, `committed`, `object_by_id`, `package_cache`, `db`}.
- `execution_cache_writes{collection}`.
- `execution_cache_expired_tickets`: read-ticket inserts that were dropped. A high rate means
  write-hot keys or generation-bucket collisions.
- `execution_cache_backpressure_status` and `_toggles`.
- `pending_notify_read`: the number of executed-effects waiters.
- `implicit_system_object_read_waits` and `_wait_latency`.

### 9.2 Hard assertions (release builds)

| Assertion | Location | Invariant |
|---|---|---|
| `fatal!("version must be monotonically increasing")` | CT:50-62 | per-key versions inserted in strictly increasing order; no duplicate writes |
| `assert_eq!(oldest, version, "version must be the oldest in the map")` | CT:110-117 | commits in causal order per object and marker |
| `panic!("dirty map must exist")` / `assert_eq!(removed, Some(value), "dirty version must exist")` | WBC:1317-1323 | every committed version was dirty, with an identical value |
| `assert!(pending_transaction_writes.remove(tx).is_some())` | WBC:1108-1114 | committed transactions were pending; `build_db_batch` filtered the unknown ones |
| `.expect("effects/events/unchanged_loaded_runtime_objects/executed effects must exist")` | WBC:1218-1236 | all per-transaction dirty entries were written together |
| `panic!("empty CachedVersionMap should have been removed")` | WBC:492 | empty version maps are always removed |
| `panic!("dirty set cannot be empty")` | WBC:1840 | same |
| `panic!("cannot have negative hit when getting latest marker")` | WBC:1936 | the latest query never produces a negative hit |
| `assert!(!latest_cache_entry.is_alive())`; `assert!(highest.is_none() \|\| tombstone)` | WBC:1888, 1893 | if neither dirty nor db has an object, no cache may claim it is live |
| `assert!(!transaction_executed_in_last_epoch)` | WBC:940-945 | no cross-epoch replay |
| `assert!(dirty.is_empty(), "cannot iterate live object set with dirty data")` | WBC:2472-2475 | live-set iteration only at epoch boundaries |
| `panic!("tombstone digest must either be deleted or wrapped")` | WBC:167 | |
| `clear_and_assert_empty` / `assert_empty` | WBC:396-425, 1415-1422 | test-only full clear |
| `take_accumulator_events` "called twice" | TO:176-181 | events are consumed once |
| `assert_eq!(prev_highest + 1, seq)` in the executor bump | CE:1072-1080 | the executed watermark is contiguous |

### 9.3 Debug-only checks

- **`object_by_id_cache` coherence** on every read (WBC:773-813): the cached latest entry must
  equal the dirty highest, else the db latest. The exception is a cached tombstone whose db rows
  were pruned (fix `2526db5d44`). This catches every bug in the class "the latest cache is stale".
- **Package cache coherence** (WBC:1553-1572): the digest equals the dirty or db package.
- **`UncommittedData::is_empty`** (WBC:279-298): when there are no pending writes, every dirty map
  is empty and `inserts == commits`.
- **`PointCacheItem::is_newer_than`**: `debug_assert_eq!` that two `Some` values are identical
  (WBC:311-315).
- **`MonotonicCache::insert`**: `debug_fatal!` if the existing entry is newer than the inserted
  value (CT:309-313). Tickets should make this impossible.
- **`clear_state_end_of_epoch`**: `debug_fatal!` if a pending transaction had shared inputs
  (WBC:1360-1369).

### 9.4 Test infrastructure worth reproducing

- `Scenario::iterate` (TESTS:110-139) runs each scenario once to count its steps, then once more
  per step with **all evictable caches cleared after that step**. Every read must give the same
  answer whatever the eviction state.
- `reset_cache` (TESTS:360-385) simulates a restart: a new cache over the same db.
- The randomized cache capacities in tests.
- Race tests: `test_concurrent_readers` (parent/child consistency),
  `latest_object_cache_race_test` (ticket monotonicity), `test_transaction_cache_race`
  (state-sync negative caching), and
  `test_get_consistent_latest_account_amount_race_with_pending_settlement`.

---

## 10. Invariants (terse)

I1. **Write-through is forbidden.** Execution outputs reach the db only through commit of a
    certified, fully executed checkpoint, plus the explicit exceptions in 3.6.
I2. **Dirty is never evicted.** It shrinks only by commit (after the db write) or by the
    end-of-epoch clear (under the execution write lock).
I3. **Versions are contiguous per key.** Dirty ∪ cached is a contiguous newest suffix of the
    key's history; every cached version < every dirty version.
I4. **No db-read value enters the version caches.** `object_cache` and `marker_cache` are filled
    only by commit.
I5. **Commit order equals causal order.** Batches are built and committed in checkpoint order;
    within a batch, versions move from dirty to cached in transaction order.
I6. **Durable before removal.** The db batch is written before any dirty entry it covers is
    removed.
I7. **Move atomically.** The dirty → cached move for a key happens with both entries locked;
    readers lock both entries in the same order.
I8. **The latest cache is monotonic.** `object_by_id_cache` is updated under the dirty entry lock
    on write; db-sourced inserts need a read ticket obtained before the lookup; an older value
    never replaces a newer one.
I9. **Ticket before read.** Every cache fill from a slower layer (dirty or db) takes its ticket
    before looking at that layer; every authoritative write uses `Ticket::Write`, which bumps the
    generation before inserting.
I10. **Point caches are insert-only.** `Some` beats `None`; two `Some` values are equal; negative
     entries are inserted only with read tickets.
I11. **Write order:** tombstones → markers → children → parents → pending outputs → effects → events
     → unchanged objects → executed digest → notify.
I12. **No dangling executed digest.** If `executed_effects_digests[tx] = D` is visible, then
     `effects[D]` is visible (in dirty, cache or db).
I13. **Register before fetch, write before notify.**
I14. **One write per transaction** (between clears); duplicates are fatal.
I15. **Revert means drop dirty**, and invalidate every non-dirty cache that may hold dirty values
     (`object_by_id_cache`, `packages`, and as a precaution `object_cache`).
I16. **Only owned-input transactions can be uncommitted at epoch end.**
I17. **The commit batch is atomic with the `highest_committed_checkpoint` watermark;** the
     executed watermark follows it. Commit tolerates already-committed transactions.
I18. **Markers are epoch-scoped**, in keys and in the db (cleared at reconfiguration).
I19. **Live-set iteration requires an empty dirty set.**
I20. **Waiters on revertible transactions are cancelled at reconfiguration.**
I21. **Settlement looks atomic to MVCC accumulator reads.** They are capped at a root version that
     is stable for the whole read.
I22. **Backpressure stops new consensus work** when the pending count exceeds the threshold, unless
     the executor has caught up with every certified checkpoint.

---

## 11. Bug history

The history below was found with `git log` and `git show` on the cache files and their
collaborators. Run these from the main checkout `/Users/marklogan/projects/mlogan-anchovy`; the
`anchovy/` worktree does not see this history. Each entry gives what went wrong, why, how it was
fixed, and the invariant it implies. Entries marked *(design)* are semantic changes rather than
bug fixes.

### 11.A Before and around the switch (2024-01 to 2024-11)

- *(design)* `d6a9015f16` (#15891, 2024-01-26): `ExecutionCache` traits with a passthrough
  "cache" over `AuthorityStore`. `eeb2f07722` (#16089) routes all db access through the traits.
  `ae35d6a6fd` (#16109) renames `in_mem_execution_cache` → `execution_cache`. `1d70f050c1`
  (#16121): "Thou shalt not convert Err => None". `c9aa1ea14a` (#16234) separates
  `ExecutionCacheCommit` (commit) from `write_transaction_outputs` (write), and passes
  `Arc<TransactionOutputs>`. The passthrough cache wrote each transaction to the db as it
  executed; `commit` was a no-op. Its `clear_state_end_of_epoch` cleared the marker table, and
  reverting required per-transaction db reverts (`revert_state_update`).
- *(design)* `09c97597b1` (#16177, 2024-02-22): **Beginning of WritebackCache.** Introduces the
  dirty/cached split, `CachedVersionMap` FIFO queues with contiguous versions, `MAX_VERSIONS=3`,
  `with_locked_cache_entries`, `move_version_from_dirty_to_cache` holding both locks, the write
  order (tombstones → markers → children → parents), and the separate `packages` cache. The
  concurrent reader/writer test came from a bug in which tombstones were written after new objects.
- *(design)* `d9801b9d30` (#16370): the checkpoint executor commits after a checkpoint executes.
  This ties durability to certified checkpoints.

**B-01 Commits landed out of checkpoint order: `52f18f9a39` (#16727, 2024-03-19).**
- *What:* `d9801b9d30` committed from the per-checkpoint tasks, which run concurrently. Checkpoint
  N+1 could reach RocksDB before N.
- *Why:* commit was part of the parallel stage.
- *Fix:* commit moved into the sequential `process_executed_checkpoint` (today, pipeline stages
  `BuildDbBatch` and `CommitTransactionOutputs`), and the watermark is bumped after it.
- *Invariant:* I5 and I17. Durable state is always a prefix of checkpoints, and the watermark is
  never ahead of it.

**B-02 Restart after the final checkpoint's watermark but before reconfiguration: `4caaa72e3f`
(#16778).**
- *What:* after a crash in that window, the executor tried to execute `last+1` in the old epoch and
  asserted.
- *Fix:* `get_next_to_schedule` returns `None` (CE:259-267).
- *Invariant:* "Final checkpoint committed, reconfiguration pending" is a legal crash state.
  Reconfiguration, including the cache clear, must be re-runnable from it.

**B-03 Committed data briefly vanished: `0d29d2ccd3` (#16968, 2024-04-02).**
- *What:* the first `commit_transaction_outputs` removed the transaction from
  `pending_transaction_writes` **before** awaiting the db write. A reader in between found it in
  neither place.
- *Fix:* write the db, insert into the cache, then remove from dirty. The comment that still
  survives at WBC:1098-1100 dates from this fix. (The same commit added caching for transactions,
  effects and events, and the "store empty events, return None" rule.)
- *Invariant:* I6. Every item is always visible in at least one of dirty, cache or db.

**B-04 Revert panicked on a transaction that never executed; `force_reload_system_packages` loaded
stale data: `d748e751e0` (#16965).**
- *What:*
  1. `revert_state_update` did `pending_transaction_writes.remove(tx).expect(...)`, but the list
     of transactions to revert (pending consensus transactions) is a superset of those that
     executed.
  2. `force_reload_system_packages` re-read packages from the db into `packages`, overwriting a
     newer **dirty** package with the older committed one.
- *Fix:* tolerate a missing transaction (asserting it was not committed), and make the reload a
  no-op.
- *Invariant:* revert must tolerate transactions that never executed. **Never fill a cache from the
  db alone while dirty may hold something newer.**

**B-05 Partial checkpoint durability: `722a87ab77` (#17194, 2024-04-17).**
- *What:* one db batch per transaction meant a crash could leave half a checkpoint on disk.
- *Fix:* one `DBBatch` per checkpoint, written before any dirty removal.
- *Invariant:* 5.3 item 6.

**B-06 Durable checkpoint referencing non-durable transactions: `befd9ffb64` (#16973).**
- *What:* a validator persisted a locally built checkpoint whose transactions' outputs were still
  dirty. After a crash, the checkpoint referred to effects that were not in the db.
- *Fix at the time:* `persist_transactions` plus re-execution of local checkpoints at startup.
  Superseded by quarantining (`e4db4a07c3`), and the code was deleted in `ebfbc0c22b`.
- *Invariant:* anything durable that references transactions must have those transactions durable,
  **or** replayable from a durable source.

**B-07 Re-commit after a crash panicked: `6243a15f0d` (#17191, 2024-05-16).**
- *What:* if the process crashed after the commit batch but before the watermark bump, the
  checkpoint is committed again at restart. Its transactions are not dirty, so
  `panic!("Attempt to commit unknown transaction")` fired.
- *Fix:* warn and skip, now in `build_db_batch` (WBC:1067-1073).
- *Invariant:* I17, idempotent re-commit.
- *(design)* The same commit added `ProxyCache` and dynamic switching between passthrough and
  writeback per epoch. Clean caches had to be emptied on switch (`clear_caches_and_assert_empty`,
  with a 100 ns sleep because moka's `invalidate_all` ignores same-tick inserts). The proxy was
  removed in `77a1ddeed3` (#20939, 2025-01-22); `reconfigure_cache` is the no-op that remains.

**B-08 Coherence check fired after tombstone pruning: `2526db5d44` (#18068).**
- *What:* the debug check compared `object_by_id_cache` against the dirty highest, else the db
  latest. After a deleted object's tombstone was pruned from the db, the cache still (correctly)
  held the tombstone.
- *Fix:* allow "cache tombstone, db nothing".
- *Invariant:* the cache can know more than the db after pruning. Coherence checks and asserts must
  accept that.

**B-09 Revert left tombstones in `object_by_id_cache`: `b7a94e740f` (#18297, 2024-06-18).**
- *What:* revert invalidated `object_by_id_cache` only for `written` ids. A deleted or wrapped
  object stayed cached as a tombstone, so it looked dead after its deletion was reverted.
- *Fix:* also invalidate for `deleted ∪ wrapped`. `8761bf8b70` later added `object_cache`
  invalidation.
- *Invariant:* I15. When dirty state is discarded, invalidate **every** id the transaction
  touched, in every cache that can hold dirty data.

**B-10 Effects visible before the transaction: `5935eabf0e` (#18785, 2024-07-24).**
- *What:* `notify_read_executed_effects` returned, the caller read the transaction, and the
  transaction was not yet in `pending_transaction_writes`, so an assertion fired.
- *Fix:* insert into `pending_transaction_writes` before the effects, events and executed digest;
  notify last.
- *Invariant:* I11 and I12. The executed digest is the publication point, and everything reachable
  from it must already be readable.

**B-11 Assertion reachable from JSON-RPC in `find_object_lt_or_eq_version`: `982024ab17` (#18843).**
- *What:* with no dirty entry and no db row, the code asserted that the cached entry was `None`. A
  read API can hit a cached tombstone whose db rows were pruned.
- *Fix:* allow a tombstone. Also added `panic!("dirty set cannot be empty")`.
- *Invariant:* the same as B-08, applied to every read path.

**B-12 In-memory lock refcounting: `2de49a40fb` (#19526); `7c9b1d1f09` (#19320).**
- *What:* `ObjectLocks` kept in-flight locks in a `DashMap<ObjectRef, LockDetails>`. Two threads
  locking the same transaction both succeeded, and the second `clear_cached_locks` panicked with
  "lock must exist". Separately, `ObjectLocks::clear()` at the epoch boundary raced with the signing
  path.
- *Fix:* refcounted entries; lock queries read only the db (in-flight locks may still roll back);
  the signing path holds the execution-lock read guard.
- *Invariant:* in-memory lock state is cleared only under the execution write lock, and every
  user holds the read side. Lock queries see only durable locks.
- **Status:** pre-consensus locking was disabled in `22f9fc9781` (#24676) and its logic removed in
  `10436c976a`. Relevant only if a re-implementation keeps pre-consensus locks.

**B-13 Waiting for effects across reconfiguration: `5aea124ff8` (#19934, 2024-10-21).**
- *What:* a waiter resolved the digest from dirty data, then reconfiguration reverted the
  transaction, and the follow-up `multi_get_effects` returned `None` → panic.
- *Fix:* wrap these waits in `within_alive_epoch`. Move `epoch_terminated()` to just after taking
  the execution write lock and before the clear (AUTH:3837-3840). Add a doc warning (EC:540-546).
- *Invariant:* I20. A two-step read (digest, then body) breaks if dirty data is dropped between the
  steps, so cancel waiters before clearing.

**B-14 Stale `object_by_id_cache` from racing writers: `5c6968bbd4` (#19951, 2024-10-22).**
- *What:* the dirty insert and the `object_by_id_cache` insert were separate. TX1 wrote (O,1) to
  dirty and paused. TX2 read (O,1) (the scheduler does not wait for TX1 to finish), wrote (O,2) to
  both. TX1 then wrote (O,1) to `object_by_id_cache`, and "latest" went backwards.
- *Fix:* hold the dirty entry lock across both inserts (WBC:614-645).
- *Invariant:* I8.

**B-15 Concurrent executions of the same certificate: `3aac32e2e5` (#19949).**
- *What:* two executions of one certificate. The first committed and its shared-version assignments
  were deleted; the second then loaded inputs and crashed.
- *Fix:* take the per-transaction guard, **then** check `get_executed_effects`, then read inputs
  (AUTH:1555-1569).
- *Invariant:* I14. The "already executed" check happens under the per-transaction lock.

**B-16 Genesis inserts bypassed the cache: `8761bf8b70` (#19508, 2024-10-03).**
- *What:* `insert_genesis_object` and `bulk_insert_genesis_objects` wrote to the db and left stale
  or negative entries in `object_cache` and `object_by_id_cache`.
- *Fix:* invalidate both caches after the write (WBC:1399-1413). The same commit turned writeback on
  by default; `b09b0a8d36` (#20275) forced it on.
- *Invariant:* any write that bypasses dirty data must invalidate the committed caches for the
  affected ids.

**B-17 Non-atomic monotonic insert: `297913c0ad` (#20236, 2024-11-12).**
- *What:* readers used `entry().or_insert_with()` plus an `is_fresh` check to resolve races, but
  writers used plain `insert()`, which can be overwritten by an older reader value. The race check
  only works if **both** sides use the entry API.
- *Fix:* `MonotonicCache` with `IsNewer`; every insert goes through the entry API and a locked
  compare-and-replace.
- *Invariant:* every writer to a forward-only cache uses the same atomic insert-or-update primitive.

### 11.B Ticketing, negative caching, backpressure (2024-12 to 2025-03)

**B-18 Stale value cached after eviction of the newer entry: `6226f54ad8` (#20450, 2024-12-09).**
- *What:*

  | Reader | Writer |
  |---|---|
  | misses `object_by_id_cache` | |
  | misses dirty | |
  | | writes dirty |
  | reads old version from db | |
  | | writes `object_by_id_cache` |
  | | the entry is evicted |
  | inserts old version (no newer entry left to compare against) | |

  `IsNewer` cannot help when the newer entry is gone.
- *Fix:* **tickets** (5.1). A read ticket is taken before any lower-layer read; `Ticket::Write`
  bumps the generation before inserting; the ticket is checked twice (in the insert closure and
  under the entry mutex).
- *Invariant:* I9.

**B-19 Backpressure: `769f14529c` (#20430, 2024-12-13)** *(design)*.
- Pending count is `inserts - commits`. Consensus intake stalls above the threshold, but only while
  certified > executed, to avoid deadlock (8.3).

**B-20 Negative caching for point reads: `03586ce9c4` (#20315)** *(design)*, and its race,
**`11e5ec9bbe` (#20875, 2025-01-14).**
- *What:* `StateSyncAPI::insert_transaction_and_effects` updated the cache (`Ticket::Write`,
  expiring tickets) **before** writing the db.

  | Reader | Writer (state sync) |
  |---|---|
  | | updates cache, expiring tickets |
  | takes ticket | |
  | misses cache | |
  | reads db: absent | |
  | | inserts `Some` |
  | inserts `None` with a still-valid ticket → "entry is newer than value" | |
  | | writes db |

- *Fix:* write the db first, then the cache. The assertion became `debug_fatal!`.
- *Invariant:* **For any data written directly to the db (state sync, genesis), the db write must
  happen before the cache update or ticket expiry.** Then any reader with a valid ticket sees the
  new db value. Test: `test_transaction_cache_race`.

**B-21 Quarantining replaces persist-before-checkpoint: `e4db4a07c3` (#20982, 2025-02-14)**
*(design)*.
- Consensus outputs are held in memory until their checkpoint is executed and committed, and
  replayed after a crash. Only `persist_transaction` (randomness) remains as a forced persist.

**B-22 Package coherence check ignored dirty data: `205ef11e0a` (#21232, 2025-02-15).**
- *What:* the debug check compared `packages` against the db; a newly published package is only in
  dirty and `packages`. The same fix made the package-miss path go through `get_object_impl`
  (dirty, then db), because a dirty package can be evicted from `packages` before it is committed.
- *Invariant:* the source of truth is dirty, then db, everywhere, including coherence checks.

**B-23 `NonExistent` cached for deleted objects: `3042cddd67` (#21497, 2025-03-18).**
- *What:* on a miss, `get_object_impl` used `store.get_object`, which returns `None` for a deleted
  or wrapped object, and cached `NonExistent`. `find_object_lt_or_eq_version` treats `NonExistent`
  as "no version ever existed" and so returned `None` for bounded reads that should have found an
  older live version (e.g. child reads at an older bound).
- *Fix:* read `get_latest_object_or_tombstone` and cache the versioned tombstone. The same commit
  added `randomize_cache_capacity_in_tests`.
- *Invariant:* `NonExistent` means no version exists anywhere. Tombstones must be cached as
  versioned tombstones.

**B-24 Tombstone downgraded to `NonExistent`: `2ff990ef00` (#21543).**
- *What:* nothing in dirty or db (pruned), `object_by_id_cache` holds a tombstone, and the code
  inserted `NonExistent` (which is "older") → `debug_fatal`.
- *Fix:* if a latest entry exists, assert `!is_alive()` and leave it; insert `NonExistent` only if
  there is no entry (WBC:1886-1899).
- *Invariant:* never downgrade a tombstone to `NonExistent`.

**B-25 Events shared across transactions: `c023028e52` (2025-03-21).**
- *What:* events were keyed by `TransactionEventsDigest` (content-addressed, refcounted in dirty).
  Two transactions with identical events shared a key, and pruning one deleted the other's events.
- *Fix:* key by transaction digest everywhere (`events_2`, dirty, cached).
- *Invariant:* per-transaction data is keyed by transaction digest, never by a content hash.

**B-26 Split batch building from writing: `84e622208e` (#21526)** *(design)*.
- Allows the executor to append rows (the watermark, B-35) and to keep CPU work out of the write.

### 11.C Markers, notifications, scheduler (2025-04 to 2025-09)

**B-27 Missing marker for transfer-to-consensus: `22aa20157f` (#21783, 2025-04-10).**
- *What:* transferring a fastpath object to consensus wrote no marker, so receiving-object
  availability did not see the old version as consumed.
- *Fix:* emit `FastpathStreamEnded` for `transferred_to_consensus()` (TO:92-108), and check "latest
  marker ≥ version" through the cache (dirty → cached → db).
- *Invariant:* markers are derived entirely from the outputs, are visible from dirty data
  immediately, and are keyed by the exact `FullObjectKey`.

**B-28 ConsensusAddressOwner transfers: `01c24bcbbd` (#22270).**
- *Fix:* emit `ConsensusStreamEnded(tx)` for `consensus_owner_changed()` as well, keyed by the
  input's `full_id()`, which includes its start version.
- *Invariant:* keep the exact `(FullObjectID including start version, version)` marker keying.

**B-29 Input-object notify in the cache: `ed70303929` (#21995)** *(design)*.
- `object_notify_read` was added; writes notify `VersionedObject` and `Package` keys, and
  `ConsensusStreamEnded` markers notify too (2.6). `5cad056f0c` (#18129) had already established
  register-then-fetch in `NotifyRead` (I13). `ce8452129a` (#17211) is the earlier TransactionManager
  lost-wakeup/reset race that motivated it.

**B-30 Fastpath outputs bypassed `commit_certificate`: `08fa2833a9` (#22733, 2025-07-16).**
- *What:* the Mysticeti fastpath output buffer was flushed by calling `write_transaction_outputs`
  directly, which skipped `insert_executed_in_epoch`.
- *Fix:* remove the bypass. Fastpath itself was removed in `9726694648`.
- *Invariant:* exactly **one** path publishes outputs to the cache (`commit_certificate`), and epoch
  bookkeeping hangs off it, before the cache write.

**B-31 Per-epoch config markers, reverted: `7ccbb5c62e` → `25fde34248` (#22499).**
- *What:* `ConfigUpdate` markers used a fixed key at `SequenceNumber::MIN`. The cache had to skip
  duplicate dirty inserts (breaking monotonic insertion), and `build_db_batch` had to read before
  writing.
- *Fix:* reverted.
- *Invariant:* every marker or object version in `TransactionOutputs` is inserted into dirty
  **exactly once**; otherwise the FIFO pop at commit fails. Marker db writes are blind puts.

**B-32 Marker table never pruned under writeback: `e98196c5db` (#22832, 2025-07-22).**
- *What:* only the passthrough cache's epoch clear called `clear_object_per_epoch_marker_table`.
  When passthrough was deleted, marker rows stopped being pruned.
- *Fix:* call it from the writeback clear, under the execution write guard (WBC:1393-1396).
- *Invariant:* the epoch reset also clears durable epoch-scoped markers.

**B-33 Revert replaced by full clear: `bca8a306df` (#23204, 2025-08-21)** *(design)*.
- `revert_state_update` and `revert_uncommitted_epoch_transactions` were deleted.
  `clear_state_end_of_epoch` drops all dirty data, with `debug_fatal!` if any remaining transaction
  has shared inputs.

**B-34 `unchanged_loaded_runtime_objects` empty vs. absent: `feaeb1fb2d` (#23818).**
- *What:* dirty stores empty lists, but the db omits them. After commit, a reader got `None`
  instead of an empty list and errored.
- *Fix:* `unwrap_or_default` at the consumer.
- *Invariant:* where the db elides empty values, readers must not be able to tell "empty" from
  "absent" (and events have the same rule built in, 1.7).

### 11.D Accumulators, crash consistency, and recent work (2025-10 to 2026-09)

**B-35 Settlement event take-once: `5c2fd3d401` (#25144).**
- *What:* each settlement root received all of the checkpoint's effects so far, so a second take
  silently returned an empty `Some(vec![])` and lost events.
- *Fix:* `Mutex<Option<Vec>>`, and a second take panics.
- *Invariant:* take-once. `None` means "no longer in the cache", never "empty".

**B-36 Barrier dependencies skipped on the fast path: `1d50231caa` (#24380); barrier already
executed via checkpoint: `08d5d54c4f` (#25408).**
- *Fix:* check `multi_get_executed_effects_digests(barrier_deps)` before the fast path. Fire an
  in-memory, **unpersisted** `notify_barrier_executed` after the cache write (AUTH:1863-1872).
- *Invariant:* notifications derived from dirty state are never persisted ahead of the commit.

**B-37 Effects pruned between the digest and the body: `2ddbe44f8e` (#23022), reverted in
`6b0439b7bd`; then `77e74b01af` (2026-01-30).**
- *What:* `notify_read_executed_effects` resolved the digest, but the effects had been pruned, so
  `fatal!`.
- *Fix:* `notify_read_executed_effects_may_fail` for historical and client paths; the panicking
  variant is kept for checkpoint building and consensus.
- *Invariant:* the two-step lookup is not atomic against pruning.

**B-38 Scheduler read latest instead of exact version: `f205eb2a43` (#24955); account read raced
with settlement: `0192aba594` (#26379); refinements `84ae728bdc`, `8c1a5dbc40`, `79585efd9e`,
`147abcdf7f`.**
- *What:* settlement writes the account at V+1 (possibly a tombstone) before the barrier bumps the
  root from V to V+1. A reader saw the tombstone with a stable root V and returned `(0, V)`.
- *Fix:* an MVCC read capped at the root version, with a pre/post root-stability retry (4.12).
  Regression test at TESTS:1166-1230.
- *Invariant:* I21. Version-specific decisions read that version; a newer tombstone means "look
  further back", not "absent".

**B-39 Replay: negative caching in a side cache: `6914bde36b` (#26337), `11f03c853c` (#26011).**
- *What:* during replay, a coin reservation resolved an account before the settlement that created
  it, and a resolver-side moka cache cached "not found".
- *Fix:* do not cache errors; add the accumulator root at the assigned version as a scheduling
  input.
- *Invariant:* side caches must not negatively cache objects that may still appear; use tickets or
  don't cache negatives.

**B-40 Drain-boundary quarantine commit: `1ad56a6b81` (#25442, 2026-02-26).**
- *What:* the quarantine released a consensus commit whose transactions were still pending roots
  for a later checkpoint. After a crash those transactions were neither durable nor replayable.
- *Fix:* release only up to the last "checkpoint queue drained" output.
- *Invariant:* 7.5 item 6.

**B-41 Crash between the builder summary and the watermark: `77bebabad1` (#24998).**
- *What:* startup deleted state hashes above `last_executed`, including one for a checkpoint whose
  builder summary was already persisted, so the executor waited forever.
- *Fix:* clean up above `max(last persisted summary, last executed)`.
- *Invariant:* 7.5 item 5.

**B-42 Watermark not atomic with outputs: `94bfdb24ad` (2026-06-07).**
- *What:* the outputs batch and `highest_executed` are written separately. A crash in between left
  checkpoint N's objects durable with the watermark at N-1. The embedded rpc-store restore then
  re-applied N, double-counting additive indexes.
- *Fix:* `highest_committed_checkpoint` written in the **same** batch
  (`set_highest_committed_checkpoint_in_batch`).
- *Invariant:* I17. Any watermark that describes durable state is written atomically with that
  state.

**B-43 Randomness signal before commit: `2de66e19de` (#21031).**
- *What:* the randomness reporter was notified before the commit. The validator stopped
  contributing partial signatures while the RandomnessStateUpdate outputs were still dirty, and
  after a crash the round could not complete.
- *Fix:* notify after `commit_transaction_outputs` (CE:482-492).
- *Invariant:* 7.5 item 7.

**B-44 Checkpoint data and index ordering: `873c865ddb`; `874da38e39`.**
- *What:* index updates were committed out of order. Separately, input keys derived from shared
  inputs included versions that never existed (deleted or cancelled), and the by-key read
  returned `None` → panic.
- *Fix:* commit indexes sequentially; use `modified_at_versions()`.
- *Invariant:* by-key reads return `None` for versions never written (markers are not objects).
  Old `modified_at` versions must remain readable (dirty, cache, or db) until checkpoint data is
  built.

**Other:**
- `29c9e8acb0` (#22135): do not evict per-digest side state at commit if a slower producer
  (consensus) can still insert it. Checkpoint execution can outrun consensus.
- `d9c99faa96` (#27062): forked outputs are rejected with `fatal!` **before** `commit_certificate`,
  so divergent outputs never reach the dirty set.
- `a9e9a9ee9c` (#25937): parallel metadata flush, sequential object flush (3.3).
- `b8a3bbc7ad` (#27523): blocking implicit system-object read (4.11). Its wait condition is
  "latest ≥ v" so that a pruned or superseded version cannot cause an infinite wait.

### 11.E Latent issues noticed while writing this (not fixed in history)

1. **Clear versus concurrent reader** (6.2). Invalidation happens before `dirty.clear()`, and
   `invalidate` does not expire tickets. Not observed in practice.
2. **`insert_genesis_object_impl`** invalidates **before** the db write, the opposite of the B-20
   rule. It is harmless only because it runs at genesis.
3. **`find_object_lt_or_eq_version`** reads the db while holding the dirty shard lock (TODO at
   WBC:1842). That costs throughput, but it is also what makes taking the ticket at the last moment
   safe there. Changing one requires changing the other.
4. **Receiving-object waiters** are only woken by a write at the exact version (4.10).
5. **Metric labels swapped** in `find_object_lt_or_eq_version` (WBC:1812-1813).
6. **`persist_transaction`** does not touch `cached.transactions`, so a negative entry cached
   earlier for that digest would hide it until eviction (3.6).

---

## 12. Incidental versus essential

### 12.1 Essential to the behavior (keep these in any re-implementation)

- Separating *executed* (visible to subsequent execution) from *durable* (in the db). The cache
  must let execution, scheduling and signing see uncommitted results immediately.
- Durability is driven by an external finality signal (here, a certified checkpoint), applied in
  a total order consistent with causality.
- Per-key version history with contiguous in-memory suffixes, so that negative and bounded
  lookups can be answered without the db; the dirty → cached move must be atomic to readers.
- The latest-version cache with monotonic, ticketed fills.
- Insert-only point caches with ticketed negative entries.
- Write ordering for child-object (bounded read) consistency.
- Notification semantics (register-then-fetch) for "executed" and "input available".
- Revert of non-final state at an epoch (configuration) boundary, by dropping dirty data and
  invalidating derived caches.
- Crash recovery by re-execution from the last durable watermark, with an atomic
  outputs-plus-watermark commit and idempotent re-commit.
- Backpressure on dirty-set growth, with deadlock avoidance.

### 12.2 Incidental to Sui's architecture

- **Validators execute before certification; fullnodes execute certified checkpoints.** Validators
  run consensus, execute transactions as soon as they are sequenced, then build and sign
  checkpoints; the cache commits only when the certified checkpoint has been "executed" (for
  validators, mostly a verification pass, CE:538-629). Fullnodes execute transactions out of
  synced checkpoints and commit immediately after. In both cases the trigger is the same (a
  certified checkpoint whose transactions are all executed); only how the transactions came to be
  executed differs. A system where execution follows finality could commit right after execution
  but would still need most of the read-side design (concurrent readers, negative caching,
  tickets).
- **RocksDB or Tidehunter and their specific tables**: `objects` keyed by `(id, version)` with
  tombstone values; `events_2`; `executed_effects`; the relocation-filter ordering of effects
  before dependents (AS:742-752); and `schedule_delete_all` for markers.
- **Markers and their variants** are artifacts of Sui's object model: receiving owned objects,
  consensus object stream ending, and smearing. A different object model needs some way to tell
  "this version will never exist" apart from "not yet".
- **Packages as immutable single-version objects**, and the `MoveObjectAsPackage` error.
- **Child objects / dynamic fields** and the lamport-timestamp version bound. The
  "children before parents" rule is essential *if* the system has bounded child reads.
- **Accumulators / address balances**: `take_accumulator_events`, MVCC reads at the root version,
  `executed_transaction_digests` for cross-epoch replay protection.
- **Implicitly read system objects** with blocking waits.
- **`persist_transaction` for randomness**, the deprecated `CheckpointCache` passthrough methods,
  `get_object_ref_prior_to_key_deprecated`, `force_reload_system_packages`,
  `reconfigure_cache`, and the no-op `ObjectLocks::clear`: vestigial or protocol-version
  compatibility.
- **moka/DashMap specifics**: segment and shard counts, and the TinyLFU policy. Any concurrent map
  with per-key locking and bounded caches with per-key atomic update will do.
- **The `ExecutionCacheTraitPointers` fat-pointer bundle** (EC:57-112): a Rust trait-object
  artifact.
- **Checkpoint executor pipeline stages**: an implementation detail of how commit order is
  enforced. The requirement is only "commit in checkpoint order".
