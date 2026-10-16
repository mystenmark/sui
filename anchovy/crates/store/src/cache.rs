// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The writeback cache (`docs/writeback_cache_design.md`): the only way to
//! the database. Transaction outputs are written here (`write`), visible to
//! every reader at once, and reach the database only when the certified
//! checkpoint holding them commits (`commit_checkpoint`), one batch per
//! checkpoint.
//!
//! Objects: one slot per id holds its uncommitted versions, its newest
//! committed versions, and its latest version as read from the database,
//! under one lock. Its uncommitted and committed versions together are a
//! contiguous newest run of its versions, which answers exact and bounded
//! reads from memory, negatively too. Transactions: one record per
//! uncommitted transaction; committed ones in a bounded cache, with
//! negative entries.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::effects::{TransactionEffects, TransactionEvents};
use messages::object::{Object, Owner};
use messages::transaction::SenderSignedData;

use crate::db::{self, Db};
use crate::{Commit, Live, Removal, Removed, Result, Written};

/// How many committed versions a slot keeps (the reference's `MAX_VERSIONS`).
const COMMITTED_VERSIONS: usize = 3;
const SHARDS: usize = 64;
/// Shards of a map, by key hash.
type Shards<K, V> = Box<[Mutex<HashMap<K, V>>]>;

/// Ticket generations per structure. Collisions only cost cache fills.
const GENERATIONS: usize = 16 * 1024;

/// How much committed data the cache keeps.
#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    /// Object ids with nothing uncommitted.
    pub objects: usize,
    /// Committed transactions, and effects by digest, each.
    pub transactions: usize,
}

impl Default for Capacity {
    fn default() -> Self {
        Capacity {
            objects: 100_000,
            transactions: 100_000,
        }
    }
}

/// An object version as the cache holds it.
#[derive(Clone)]
enum Entry {
    Object(Message<Object<'static>>, Digest),
    Deleted,
    Wrapped,
}

impl Entry {
    fn object(&self) -> Option<Message<Object<'static>>> {
        match self {
            Entry::Object(object, _) => Some(object.clone()),
            Entry::Deleted | Entry::Wrapped => None,
        }
    }
}

/// The latest version as the database last answered, for a slot with
/// nothing in memory.
#[derive(Clone)]
enum DbLatest {
    Version(u64, Box<Entry>),
    NonExistent,
}

#[derive(Default)]
struct Slot {
    /// Uncommitted versions, oldest first. Never evicted.
    dirty: VecDeque<(u64, Entry)>,
    /// The newest committed versions, oldest first, filled only by commit:
    /// a version read from the database could leave a gap.
    committed: VecDeque<(u64, Entry)>,
    /// Filled only by a read, and only while `dirty` and `committed` are
    /// empty, under a ticket.
    db_latest: Option<DbLatest>,
    /// For eviction: read since the clock last passed.
    referenced: bool,
}

impl Slot {
    fn in_memory(&self) -> bool {
        !self.dirty.is_empty() || !self.committed.is_empty()
    }

    fn highest(&self) -> Option<&(u64, Entry)> {
        self.dirty.back().or_else(|| self.committed.back())
    }

    fn least(&self) -> Option<u64> {
        self.committed
            .front()
            .or_else(|| self.dirty.front())
            .map(|(v, _)| *v)
    }

    /// Newest first.
    fn versions(&self) -> impl Iterator<Item = &(u64, Entry)> {
        self.dirty.iter().rev().chain(self.committed.iter().rev())
    }

    fn push_dirty(&mut self, version: u64, entry: Entry) {
        let above = match (self.highest(), &self.db_latest) {
            (Some((highest, _)), _) => Some(*highest),
            (None, Some(DbLatest::Version(v, _))) => Some(*v),
            (None, _) => None,
        };
        assert!(
            above.is_none_or(|v| version > v),
            "version {version} written after {above:?}: versions go up, once each"
        );
        self.db_latest = None;
        self.dirty.push_back((version, entry));
    }
}

/// What memory says about a read.
enum Found<T> {
    Hit(T),
    /// Known not to exist.
    Absent,
    /// Ask the database.
    Miss,
}

#[derive(Default)]
struct ObjectShard {
    slots: HashMap<ObjectId, Slot>,
    /// Eviction order (the clock).
    clock: VecDeque<ObjectId>,
}

impl ObjectShard {
    fn slot(&mut self, id: &ObjectId) -> &mut Slot {
        if !self.slots.contains_key(id) {
            self.clock.push_back(*id);
        }
        self.slots.entry(*id).or_default()
    }

    /// Evicts slots with nothing uncommitted, least recently read first,
    /// until at most `capacity` remain.
    fn evict(&mut self, capacity: usize) {
        let mut budget = 2 * self.clock.len();
        while self.slots.len() > capacity && budget > 0 {
            budget -= 1;
            let Some(id) = self.clock.pop_front() else {
                return;
            };
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            if !slot.dirty.is_empty() || std::mem::take(&mut slot.referenced) {
                self.clock.push_back(id);
            } else {
                self.slots.remove(&id);
            }
        }
    }
}

/// Generation numbers, by key hash. A reader takes one before reading a
/// slower layer; anything that could make its answer stale bumps it; the
/// reader fills a cache only if it is unchanged.
struct Generations(Box<[AtomicU64]>);

impl Generations {
    fn new() -> Generations {
        Generations((0..GENERATIONS).map(|_| AtomicU64::new(0)).collect())
    }

    fn of(&self, key: &[u8; 32]) -> &AtomicU64 {
        &self.0[hash(key) % GENERATIONS]
    }

    fn ticket(&self, key: &[u8; 32]) -> u64 {
        self.of(key).load(Ordering::Acquire)
    }

    fn bump(&self, key: &[u8; 32]) {
        self.of(key).fetch_add(1, Ordering::AcqRel);
    }
}

fn hash(key: &[u8; 32]) -> usize {
    let mut h = 0u64;
    for chunk in key.chunks_exact(8) {
        h = h.rotate_left(17) ^ u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
    }
    h.wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize
}

fn shard(key: &[u8; 32]) -> usize {
    (hash(key) >> 32) % SHARDS
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each critical section leaves its shard consistent.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A transaction's data apart from its objects.
struct TxData {
    transaction: Message<SenderSignedData<'static>>,
    effects_digest: Digest,
    effects: Message<TransactionEffects<'static>>,
    events: Option<Message<TransactionEvents<'static>>>,
}

/// An executed, uncommitted transaction.
struct TxRecord {
    digest: Digest,
    data: Arc<TxData>,
    written: Vec<(ObjectId, u64, Digest, Message<Object<'static>>)>,
    removed: Vec<Removed>,
}

/// A bounded cache of committed values by digest, with negative entries.
struct PointCache<V> {
    shards: Box<[Mutex<PointShard<V>>]>,
    generations: Generations,
    capacity: usize,
}

struct PointShard<V> {
    entries: HashMap<Digest, Option<V>>,
    order: VecDeque<Digest>,
}

impl<V: Clone> PointCache<V> {
    fn new(capacity: usize) -> PointCache<V> {
        PointCache {
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(PointShard {
                        entries: HashMap::new(),
                        order: VecDeque::new(),
                    })
                })
                .collect(),
            generations: Generations::new(),
            capacity: capacity.div_ceil(SHARDS),
        }
    }

    fn get(&self, key: &Digest) -> Found<V> {
        match lock(&self.shards[shard(&key.bytes)]).entries.get(key) {
            Some(Some(value)) => Found::Hit(value.clone()),
            Some(None) => Found::Absent,
            None => Found::Miss,
        }
    }

    fn insert(shard: &mut PointShard<V>, key: Digest, value: Option<V>, capacity: usize) {
        if shard.entries.insert(key, value).is_none() {
            shard.order.push_back(key);
        }
        while shard.entries.len() > capacity {
            let Some(old) = shard.order.pop_front() else {
                break;
            };
            shard.entries.remove(&old);
        }
    }

    /// A committed value: replaces anything, and outdates every ticket.
    fn insert_committed(&self, key: Digest, value: V) {
        let mut shard = lock(&self.shards[shard(&key.bytes)]);
        self.generations.bump(&key.bytes);
        Self::insert(&mut shard, key, Some(value), self.capacity);
    }

    /// "Not in the database", if nothing happened to `key` since `ticket`.
    fn insert_absent(&self, key: Digest, ticket: u64) {
        let mut shard = lock(&self.shards[shard(&key.bytes)]);
        if self.generations.ticket(&key.bytes) == ticket && !shard.entries.contains_key(&key) {
            Self::insert(&mut shard, key, None, self.capacity);
        }
    }

    fn invalidate(&self, key: &Digest) {
        let mut shard = lock(&self.shards[shard(&key.bytes)]);
        self.generations.bump(&key.bytes);
        shard.entries.remove(key);
    }
}

/// See the module documentation.
pub struct Store {
    db: Db,
    capacity: Capacity,
    objects: Box<[Mutex<ObjectShard>]>,
    object_generations: Generations,
    /// Uncommitted transactions, by digest and by effects digest.
    uncommitted: Shards<Digest, Arc<TxRecord>>,
    uncommitted_effects: Shards<Digest, Arc<TxData>>,
    committed: PointCache<Arc<TxData>>,
    committed_effects: PointCache<Message<TransactionEffects<'static>>>,
    /// The last checkpoint committed by this cache.
    last_committed: Mutex<Option<u64>>,
    /// Makes `start_version`'s read and initialization one step.
    start_versions: Mutex<()>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Store::open_with(path, Capacity::default())
    }

    pub fn open_with(path: &Path, capacity: Capacity) -> Result<Store> {
        let db = Db::open(path)?;
        let last_committed = db.committed_checkpoint()?;
        Ok(Store {
            db,
            capacity,
            objects: (0..SHARDS).map(|_| Mutex::default()).collect(),
            object_generations: Generations::new(),
            uncommitted: (0..SHARDS).map(|_| Mutex::default()).collect(),
            uncommitted_effects: (0..SHARDS).map(|_| Mutex::default()).collect(),
            committed: PointCache::new(capacity.transactions),
            committed_effects: PointCache::new(capacity.transactions),
            last_committed: Mutex::new(last_committed),
            start_versions: Mutex::new(()),
        })
    }

    fn objects(&self, id: &ObjectId) -> MutexGuard<'_, ObjectShard> {
        lock(&self.objects[shard(&id.0)])
    }

    // ---- Writes ----

    /// Writes one executed transaction's outputs, visible to every reader
    /// at once and kept until its checkpoint commits: tombstones, then
    /// objects owned by objects, then the rest, then the transaction (from
    /// then on, executed). Children before parents: a read bounded by a
    /// parent's version never pairs it with a child's older version.
    ///
    /// # Panics
    /// If the transaction was written before, a version is not above its
    /// object's latest, or the outputs do not parse: each a bug upstream.
    pub fn write(&self, outputs: Commit) {
        let executed = outputs.executed.expect("a transaction's outputs");
        let written: Vec<(ObjectId, u64, Digest, Message<Object<'static>>)> = outputs
            .written
            .into_iter()
            .map(|w| (w.id, w.version, w.digest, parse(w.bytes)))
            .collect();
        let data = Arc::new(TxData {
            transaction: parse(executed.transaction),
            effects_digest: executed.effects_digest,
            effects: parse(executed.effects),
            events: executed.events.map(parse),
        });

        for removed in &outputs.removed {
            let entry = match removed.removal {
                Removal::Deleted => Entry::Deleted,
                Removal::Wrapped => Entry::Wrapped,
            };
            self.push_dirty(&removed.id, removed.version, entry);
        }
        let child = |o: &Message<Object<'static>>| matches!(o.get().owner, Owner::ObjectOwner(_));
        for children in [true, false] {
            for (id, version, digest, object) in &written {
                if child(object) == children {
                    self.push_dirty(id, *version, Entry::Object(object.clone(), *digest));
                }
            }
        }

        let record = Arc::new(TxRecord {
            digest: executed.digest,
            data: data.clone(),
            written,
            removed: outputs.removed,
        });
        lock(&self.uncommitted_effects[shard(&data.effects_digest.bytes)])
            .insert(data.effects_digest, data.clone());
        let previous =
            lock(&self.uncommitted[shard(&executed.digest.bytes)]).insert(executed.digest, record);
        assert!(previous.is_none(), "{:?} written twice", executed.digest);
    }

    fn push_dirty(&self, id: &ObjectId, version: u64, entry: Entry) {
        let mut shard = self.objects(id);
        self.object_generations.bump(&id.0);
        shard.slot(id).push_dirty(version, entry);
    }

    /// Commits checkpoint `seq`: one database batch with the outputs of
    /// `transactions` (its transactions, in checkpoint order), the
    /// checkpoint's record and certified summary, and the committed
    /// watermark; then their outputs move from uncommitted to committed. A
    /// transaction not uncommitted was committed before (a commit that
    /// crashed before returning), and is skipped.
    ///
    /// # Panics
    /// If checkpoints commit out of order, or the database write fails.
    pub fn commit_checkpoint(
        &self,
        seq: u64,
        transactions: &[Digest],
        checkpoint: Vec<u8>,
        certified: Vec<u8>,
    ) {
        self.commit(seq, transactions, checkpoint, Some(certified), None);
    }

    /// Commits the genesis checkpoint, which has no transactions of its own
    /// (genesis objects are in already) and no certificate, with the first
    /// epoch's start record.
    ///
    /// # Panics
    /// If a checkpoint is committed already, or the database write fails.
    pub fn commit_genesis_checkpoint(&self, checkpoint: Vec<u8>, epoch_start: Vec<u8>) {
        self.commit(0, &[], checkpoint, None, Some(epoch_start));
    }

    fn commit(
        &self,
        seq: u64,
        transactions: &[Digest],
        checkpoint: Vec<u8>,
        certified: Option<Vec<u8>>,
        epoch_start: Option<Vec<u8>>,
    ) {
        {
            let last = *lock(&self.last_committed);
            assert_eq!(
                seq,
                last.map_or(0, |last| last + 1),
                "checkpoint {seq} committed after {last:?}"
            );
        }
        let records: Vec<Arc<TxRecord>> = transactions
            .iter()
            .filter_map(|d| lock(&self.uncommitted[shard(&d.bytes)]).get(d).cloned())
            .collect();

        // 1. Durable before anything leaves memory.
        let mut batch = self.db.write_batch();
        for record in &records {
            for removed in &record.removed {
                self.db
                    .put_tombstone(&mut batch, &removed.id, removed.version, removed.removal);
            }
            for (id, version, digest, object) in &record.written {
                self.db.put_object(
                    &mut batch,
                    id,
                    *version,
                    digest,
                    object.wire_bytes().to_vec(),
                );
            }
            let data = &record.data;
            self.db.put_executed(
                &mut batch,
                &record.digest,
                data.transaction.wire_bytes().to_vec(),
                &data.effects_digest,
                data.effects.wire_bytes().to_vec(),
                data.events.as_ref().map(|e| e.wire_bytes().to_vec()),
            );
        }
        self.db
            .put_committed_checkpoint(&mut batch, seq, checkpoint, certified);
        if let Some(epoch_start) = epoch_start {
            self.db.put_epoch_start(&mut batch, epoch_start);
        }
        batch.commit().expect("the database writes");

        // 2. Then out of memory, in checkpoint order: each object's versions
        //    leave its uncommitted run oldest first.
        for record in &records {
            for removed in &record.removed {
                self.move_to_committed(&removed.id, removed.version);
            }
            for (id, version, ..) in &record.written {
                self.move_to_committed(id, *version);
            }
            let data = &record.data;
            self.committed.insert_committed(record.digest, data.clone());
            self.committed_effects
                .insert_committed(data.effects_digest, data.effects.clone());
            lock(&self.uncommitted_effects[shard(&data.effects_digest.bytes)])
                .remove(&data.effects_digest);
            lock(&self.uncommitted[shard(&record.digest.bytes)]).remove(&record.digest);
        }
        *lock(&self.last_committed) = Some(seq);
    }

    fn move_to_committed(&self, id: &ObjectId, version: u64) {
        let mut shard = self.objects(id);
        let slot = shard
            .slots
            .get_mut(id)
            .expect("a committed version was written");
        let (oldest, entry) = slot
            .dirty
            .pop_front()
            .expect("a committed version was written");
        assert_eq!(oldest, version, "{id:?}: committed out of order");
        slot.committed.push_back((version, entry));
        while slot.committed.len() > COMMITTED_VERSIONS {
            slot.committed.pop_front();
        }
        shard.evict(self.capacity.objects.div_ceil(SHARDS));
    }

    /// Writes `commit` to the database directly, bypassing the uncommitted
    /// set: genesis, and paths before the pipeline. The database first, then
    /// whatever the cache held for its keys is dropped.
    ///
    /// # Panics
    /// If an object it writes has uncommitted versions.
    pub fn write_through(&self, commit: Commit) -> Result<()> {
        let ids: Vec<ObjectId> = commit
            .written
            .iter()
            .map(|w| w.id)
            .chain(commit.removed.iter().map(|r| r.id))
            .collect();
        let executed = commit
            .executed
            .as_ref()
            .map(|e| (e.digest, e.effects_digest));
        self.db.commit(commit)?;
        self.invalidate(&ids, executed);
        Ok(())
    }

    fn invalidate(&self, ids: &[ObjectId], executed: Option<(Digest, Digest)>) {
        for id in ids {
            let mut shard = self.objects(id);
            self.object_generations.bump(&id.0);
            if let Some(slot) = shard.slots.get_mut(id) {
                assert!(
                    slot.dirty.is_empty(),
                    "{id:?} written around its uncommitted versions"
                );
                shard.slots.remove(id);
            }
        }
        if let Some((digest, effects_digest)) = executed {
            self.committed.invalidate(&digest);
            self.committed_effects.invalidate(&effects_digest);
        }
    }

    /// Drops every uncommitted output: for a node whose uncommitted work is
    /// gone (restart, tests). Every reader's ticket is outdated, so none
    /// can cache a dropped version.
    pub fn clear_uncommitted(&self) {
        for shard in &self.objects {
            let mut shard = lock(shard);
            let ObjectShard { slots, .. } = &mut *shard;
            for (id, slot) in slots.iter_mut() {
                if !slot.dirty.is_empty() {
                    self.object_generations.bump(&id.0);
                    slot.dirty.clear();
                }
            }
            slots.retain(|_, slot| slot.in_memory() || slot.db_latest.is_some());
        }
        for shard in &self.uncommitted {
            lock(shard).clear();
        }
        for shard in &self.uncommitted_effects {
            lock(shard).clear();
        }
    }

    /// How many transactions are written and not committed.
    pub fn uncommitted_transactions(&self) -> usize {
        self.uncommitted.iter().map(|s| lock(s).len()).sum()
    }

    // ---- Object reads ----

    /// The object at exactly `version`; none if there is none, or a
    /// tombstone.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn object(&self, id: &ObjectId, version: u64) -> Result<Option<Message<Object<'static>>>> {
        let found = {
            let mut shard = self.objects(id);
            match shard.slots.get_mut(id) {
                None => Found::Miss,
                Some(slot) => {
                    slot.referenced = true;
                    exact(slot, version)
                }
            }
        };
        match found {
            Found::Hit(entry) => Ok(entry.object()),
            Found::Absent => Ok(None),
            Found::Miss => Ok(self.db.object(id, version)?),
        }
    }

    /// The object's latest version, live or a tombstone; none if it never
    /// existed.
    fn latest(&self, id: &ObjectId) -> Result<Option<(u64, Entry)>> {
        let ticket = {
            let mut shard = self.objects(id);
            if let Some(slot) = shard.slots.get_mut(id) {
                slot.referenced = true;
                if let Some((version, entry)) = slot.highest() {
                    return Ok(Some((*version, entry.clone())));
                }
                match &slot.db_latest {
                    Some(DbLatest::Version(version, entry)) => {
                        return Ok(Some((*version, (**entry).clone())));
                    }
                    Some(DbLatest::NonExistent) => return Ok(None),
                    None => {}
                }
            }
            self.object_generations.ticket(&id.0)
        };
        let latest = self.db.latest(id)?.map(|(version, latest)| {
            let entry = match latest {
                db::Latest::Object(object, digest) => Entry::Object(object, digest),
                db::Latest::Deleted => Entry::Deleted,
                db::Latest::Wrapped => Entry::Wrapped,
            };
            (version, entry)
        });
        let mut shard = self.objects(id);
        if self.object_generations.ticket(&id.0) == ticket {
            let slot = shard.slot(id);
            if !slot.in_memory() && slot.db_latest.is_none() {
                slot.db_latest = Some(match &latest {
                    Some((version, entry)) => DbLatest::Version(*version, Box::new(entry.clone())),
                    None => DbLatest::NonExistent,
                });
            }
            shard.evict(self.capacity.objects.div_ceil(SHARDS));
        }
        Ok(latest)
    }

    /// The version and digest of the object's live version.
    pub fn live(&self, id: &ObjectId) -> Result<Option<Live>> {
        Ok(match self.latest(id)? {
            Some((version, Entry::Object(_, digest))) => Some(Live { version, digest }),
            _ => None,
        })
    }

    /// The object's live version.
    pub fn live_object(&self, id: &ObjectId) -> Result<Option<Message<Object<'static>>>> {
        Ok(self.latest(id)?.and_then(|(_, entry)| entry.object()))
    }

    /// The object's highest version at or below `bound`, live or not; none
    /// if that is a tombstone (sui's `find_object_lt_or_eq_version`).
    pub fn object_at_or_before(
        &self,
        id: &ObjectId,
        bound: u64,
    ) -> Result<Option<Message<Object<'static>>>> {
        {
            let mut shard = self.objects(id);
            if let Some(slot) = shard.slots.get_mut(id) {
                slot.referenced = true;
                if let Some((_, entry)) = slot.versions().find(|(v, _)| *v <= bound) {
                    return Ok(entry.object());
                }
                if !slot.in_memory() {
                    match &slot.db_latest {
                        Some(DbLatest::Version(v, entry)) if *v <= bound => {
                            return Ok(entry.object());
                        }
                        Some(DbLatest::NonExistent) => return Ok(None),
                        _ => {}
                    }
                }
            }
        }
        // Every version in memory is above `bound`, so the answer is one
        // the database holds, and no later write or commit changes it.
        self.db.object_at_or_before(id, bound)
    }

    // ---- Transaction reads ----

    fn uncommitted(&self, digest: &Digest) -> Option<Arc<TxData>> {
        lock(&self.uncommitted[shard(&digest.bytes)])
            .get(digest)
            .map(|r| r.data.clone())
    }

    /// The transaction's data from memory: uncommitted, else committed and
    /// cached. A miss is answered from the database, each read its own
    /// fields; only "not executed" is cached.
    fn cached_tx(&self, digest: &Digest) -> Found<Arc<TxData>> {
        if let Some(data) = self.uncommitted(digest) {
            return Found::Hit(data);
        }
        self.committed.get(digest)
    }

    pub fn transaction(
        &self,
        digest: &Digest,
    ) -> Result<Option<Message<SenderSignedData<'static>>>> {
        match self.cached_tx(digest) {
            Found::Hit(data) => Ok(Some(data.transaction.clone())),
            // Absent means not executed; a transaction is stored only executed.
            Found::Absent => Ok(None),
            Found::Miss => Ok(self.db.transaction(digest)?),
        }
    }

    /// The digest of the effects of the transaction, if it executed.
    pub fn executed_effects(&self, transaction: &Digest) -> Result<Option<Digest>> {
        let ticket = self.committed.generations.ticket(&transaction.bytes);
        match self.cached_tx(transaction) {
            Found::Hit(data) => Ok(Some(data.effects_digest)),
            Found::Absent => Ok(None),
            Found::Miss => {
                let found = self.db.executed_effects(transaction)?;
                if found.is_none() {
                    self.committed.insert_absent(*transaction, ticket);
                }
                Ok(found)
            }
        }
    }

    pub fn effects(&self, digest: &Digest) -> Result<Option<Message<TransactionEffects<'static>>>> {
        let ticket = self.committed_effects.generations.ticket(&digest.bytes);
        if let Some(data) = lock(&self.uncommitted_effects[shard(&digest.bytes)]).get(digest) {
            return Ok(Some(data.effects.clone()));
        }
        match self.committed_effects.get(digest) {
            Found::Hit(effects) => return Ok(Some(effects)),
            Found::Absent => return Ok(None),
            Found::Miss => {}
        }
        let found = self.db.effects(digest)?;
        if found.is_none() {
            self.committed_effects.insert_absent(*digest, ticket);
        }
        Ok(found)
    }

    pub fn events(
        &self,
        transaction: &Digest,
    ) -> Result<Option<Message<TransactionEvents<'static>>>> {
        match self.cached_tx(transaction) {
            Found::Hit(data) => Ok(data.events.clone()),
            Found::Absent => Ok(None),
            Found::Miss => Ok(self.db.events(transaction)?),
        }
    }

    // ---- The database's own records ----

    /// Whether genesis is in.
    pub fn has_genesis(&self) -> Result<bool> {
        self.db.has_genesis()
    }

    /// Genesis: its objects, and the mark `has_genesis` reads, together.
    pub fn commit_genesis(&self, objects: Vec<Written>) -> Result<()> {
        let ids: Vec<ObjectId> = objects.iter().map(|w| w.id).collect();
        self.db.commit_genesis(objects)?;
        self.invalidate(&ids, None);
        Ok(())
    }

    /// Committed checkpoint `seq`, as the pipeline encodes it.
    pub fn checkpoint(&self, seq: u64) -> Result<Option<Vec<u8>>> {
        self.db.checkpoint(seq)
    }

    /// Committed checkpoint `seq`'s certified summary.
    pub fn certified(&self, seq: u64) -> Result<Option<Vec<u8>>> {
        self.db.certified(seq)
    }

    /// The highest committed checkpoint, and its sequence number.
    pub fn last_checkpoint(&self) -> Result<Option<(u64, Vec<u8>)>> {
        self.db.last_checkpoint()
    }

    /// The current epoch's start record: the one durable point at which the
    /// node is in that epoch.
    pub fn epoch_start(&self) -> Result<Option<Vec<u8>>> {
        self.db.epoch_start()
    }

    /// Replaces the current epoch's start record, moving the node into the
    /// epoch it describes.
    ///
    /// # Panics
    /// If the database write fails.
    pub fn commit_epoch_start(&self, record: Vec<u8>) {
        let mut batch = self.db.write_batch();
        self.db.put_epoch_start(&mut batch, record);
        batch.commit().expect("the database writes");
    }

    /// The version and digest version assignment starts shared object `id`
    /// (shared at `initial_shared_version`) at in `epoch`: as recorded the
    /// first time a version was assigned to it in the epoch, else its live
    /// version, recorded now. None if it has neither.
    ///
    /// The live version is right only before anything assigned in the
    /// epoch has written the object, which the first assignment is. It is
    /// recorded durably at once, so before any such write can commit: a
    /// replay of the epoch finds it, though the store has moved on.
    pub fn start_version(
        &self,
        epoch: u64,
        id: &ObjectId,
        initial_shared_version: u64,
    ) -> Result<Option<Live>> {
        let _initializing = lock(&self.start_versions);
        if let Some(start) = self.db.start_version(epoch, id, initial_shared_version)? {
            return Ok(Some(start));
        }
        let Some(live) = self.live(id)? else {
            return Ok(None);
        };
        self.db
            .put_start_version(epoch, id, initial_shared_version, live)?;
        Ok(Some(live))
    }

    /// The highest checkpoint whose outputs are all in the database.
    pub fn committed_checkpoint(&self) -> Result<Option<u64>> {
        self.db.committed_checkpoint()
    }
}

/// An exact read of `version` from memory.
fn exact(slot: &Slot, version: u64) -> Found<Entry> {
    if let Some((_, entry)) = slot.versions().find(|(v, _)| *v == version) {
        return Found::Hit(entry.clone());
    }
    if let Some(least) = slot.least() {
        // In or above the contiguous run, and not in it: never existed, or
        // not yet.
        if version > least {
            return Found::Absent;
        }
        return Found::Miss;
    }
    match &slot.db_latest {
        Some(DbLatest::Version(latest, entry)) if *latest == version => {
            Found::Hit((**entry).clone())
        }
        Some(DbLatest::Version(latest, _)) if version > *latest => Found::Absent,
        Some(DbLatest::NonExistent) => Found::Absent,
        _ => Found::Miss,
    }
}

/// Outputs the executor produced parse: anything else is a bug.
fn parse<T: messages::Parse>(bytes: Vec<u8>) -> Message<T> {
    Message::parse(bytes)
        .map_err(|(e, _)| e)
        .expect("execution's outputs parse")
}
