// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store: what the pipeline keeps for one epoch, discarded once
//! it is over (`docs/two_stores_design.md`). Over the epoch's database
//! (`store::epoch`), in three sections: consensus output, written only by
//! the commit handler; checkpoint builder state; and everything else.

use std::collections::BTreeMap;

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use containers::{HeapMessageMap, HeapMessageSet};
use hashbrown::HashMap;
use messages::base::{Digest, Hashed, ObjectId, ObjectRef};
use store::Live;
use store::epoch::{EpochDb, Space};

use crate::consensus::deferral::DeferralKey;
use crate::pipeline::keys::{StreamId, TxDigest};
use crate::pipeline::tables::Generation;

/// Readers of every section, and the writes of the last. Cloned handles
/// share the database.
#[derive(Clone)]
pub struct EpochStore {
    db: Arc<EpochDb>,
    /// Makes `start_version`'s read and initialization one step.
    start_versions: Arc<Mutex<()>>,
}

impl EpochStore {
    /// Epoch `epoch`'s store under `root`: made at the epoch's start,
    /// reopened as it was on restart. With it, the one writer of its
    /// consensus output, for the commit handler, and the one writer of its
    /// checkpoint builder state.
    ///
    /// # Panics
    /// If the database cannot be opened.
    pub fn open(root: &Path, epoch: u64) -> (EpochStore, ConsensusOutput, BuilderOutput) {
        let db = Arc::new(EpochDb::open(root, epoch).expect("the epoch's database opens"));
        let deferred = db
            .entries(Space::Deferred)
            .expect("the epoch's database reads")
            .into_iter()
            .map(|(key, value)| {
                let key = key.try_into().expect("a deferral key's length");
                (DeferralKey::decode(&key), value)
            })
            .collect();
        let consensus = ConsensusOutput {
            quarantine: Arc::new(Mutex::new(Quarantine {
                db: db.clone(),
                deferred,
                open: Output::default(),
                queued: std::collections::VecDeque::new(),
                processed: containers::heap_message_map(0),
                locks: SipMap::default(),
                streams: SipMap::default(),
                committed_height: 0,
            })),
        };
        let builder = BuilderOutput { db: db.clone() };
        let store = EpochStore {
            db,
            start_versions: Arc::new(Mutex::new(())),
        };
        (store, consensus, builder)
    }

    pub fn epoch(&self) -> u64 {
        self.db.epoch()
    }

    /// The version and digest version assignment starts shared object `id`
    /// (shared at `initial_shared_version`) at in the epoch: as recorded the
    /// first time a version was assigned to it in the epoch, else its live
    /// version in `objects`, recorded now. None if it has neither.
    ///
    /// The live version is right only before anything assigned in the epoch
    /// has written the object, which the first assignment is. It is recorded
    /// durably at once, so before any such write can commit: a replay of the
    /// epoch finds it, though the object store has moved on.
    ///
    /// # Panics
    /// If a database cannot be read or written.
    pub fn start_version(
        &self,
        objects: &store::Store,
        id: &ObjectId,
        initial_shared_version: u64,
    ) -> Option<Live> {
        let _initializing = self
            .start_versions
            .lock()
            .expect("the lock is not poisoned");
        let key = stream_key(id, initial_shared_version);
        if let Some(value) = self
            .db
            .get(Space::StartVersions, &key)
            .expect("the epoch's database reads")
        {
            return Some(Live {
                version: u64::from_le_bytes(value[..8].try_into().expect("eight bytes")),
                digest: Digest::new(value[8..].try_into().expect("a digest")),
            });
        }
        let live = objects.live(id).expect("the store reads")?;
        let mut value = live.version.to_le_bytes().to_vec();
        value.extend_from_slice(&live.digest.bytes);
        let mut batch = self.db.batch();
        batch.put(Space::StartVersions, key.to_vec(), value);
        batch.commit().expect("the epoch's database writes");
        Some(live)
    }
}

impl EpochStore {
    /// Records generations: `(stream, round, generation, value)`.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn put_generations(&self, generations: &[(StreamId, u32, u32, Generation)]) {
        let mut batch = self.db.batch();
        for &(stream, round, generation, value) in generations {
            let (tag, version, bytes) = match value {
                Generation::Version { version, digest } => (0, version, digest),
                Generation::Ended {
                    version,
                    transaction,
                } => (1, version, transaction),
            };
            let mut encoded = vec![tag];
            encoded.extend_from_slice(&version.to_le_bytes());
            encoded.extend_from_slice(&bytes);
            batch.put(
                Space::Generations,
                generation_key(&stream, round, generation).to_vec(),
                encoded,
            );
        }
        batch.commit().expect("the epoch's database writes");
    }

    /// Generation `generation` of `stream` in `round`, if recorded.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn generation(&self, stream: &StreamId, round: u32, generation: u32) -> Option<Generation> {
        let v = self
            .db
            .get(
                Space::Generations,
                &generation_key(stream, round, generation),
            )
            .expect("the epoch's database reads")?;
        let version = u64::from_le_bytes(v[1..9].try_into().expect("8 bytes"));
        let bytes: [u8; 32] = v[9..].try_into().expect("32 bytes");
        Some(match v[0] {
            0 => Generation::Version {
                version,
                digest: bytes,
            },
            _ => Generation::Ended {
                version,
                transaction: bytes,
            },
        })
    }

    /// Checkpoint `seq`, built here, as the builder wrote it.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn built(&self, seq: u64) -> Option<Vec<u8>> {
        self.db
            .get(Space::Built, &seq.to_be_bytes())
            .expect("the epoch's database reads")
    }

    /// Checkpoint `seq`'s state hash, as the builder wrote it.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn checkpoint_hash(&self, seq: u64) -> Option<Vec<u8>> {
        self.db
            .get(Space::CheckpointHashes, &seq.to_be_bytes())
            .expect("the epoch's database reads")
    }

    /// The root after checkpoint `seq`, as the builder wrote it.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn root(&self, seq: u64) -> Option<Vec<u8>> {
        self.db
            .get(Space::Roots, &seq.to_be_bytes())
            .expect("the epoch's database reads")
    }
}

/// The epoch's checkpoint builder state, written through this writer only:
/// it is made once, with the store, and cloned only for the checkpoint
/// builder's parts.
#[derive(Clone)]
pub struct BuilderOutput {
    db: Arc<EpochDb>,
}

impl BuilderOutput {
    /// Records checkpoint `seq`, built here, encoded.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn put_built(&self, seq: u64, checkpoint: Vec<u8>) {
        self.put(Space::Built, seq, checkpoint);
    }

    /// Records checkpoint `seq`'s state hash.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn put_checkpoint_hash(&self, seq: u64, hash: Vec<u8>) {
        self.put(Space::CheckpointHashes, seq, hash);
    }

    /// Records the root after checkpoint `seq`.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn put_root(&self, seq: u64, root: Vec<u8>) {
        self.put(Space::Roots, seq, root);
    }

    /// Deletes the state hashes and roots of checkpoints above `seq`, the
    /// committed one: at startup, so a checkpoint rebuilt differently, after
    /// a fork, cannot keep a hash its old contents gave
    /// (`state_hash_by_checkpoint`'s cleanup in the reference).
    ///
    /// # Panics
    /// If the database cannot be read or written.
    pub fn delete_state_hashes_above(&self, seq: u64) {
        let mut batch = self.db.batch();
        for space in [Space::CheckpointHashes, Space::Roots] {
            for above in self
                .db
                .numbers_above(space, seq)
                .expect("the epoch's database reads")
            {
                batch.delete(space, above.to_be_bytes().to_vec());
            }
        }
        batch.commit().expect("the epoch's database writes");
    }

    fn put(&self, space: Space, seq: u64, value: Vec<u8>) {
        let mut batch = self.db.batch();
        batch.put(space, seq.to_be_bytes().to_vec(), value);
        batch.commit().expect("the epoch's database writes");
    }
}

/// A generation's key: its stream's, then the big-endian round and
/// generation.
fn generation_key(stream: &StreamId, round: u32, generation: u32) -> [u8; 48] {
    let mut key = [0; 48];
    key[..40].copy_from_slice(&stream_key(&stream.id, stream.initial_shared_version));
    key[40..44].copy_from_slice(&round.to_be_bytes());
    key[44..].copy_from_slice(&generation.to_be_bytes());
    key
}

/// A stream's key: its object id, then the big-endian version its sharing
/// started at.
fn stream_key(id: &ObjectId, initial_shared_version: u64) -> [u8; 40] {
    let mut key = [0; 40];
    key[..32].copy_from_slice(&id.0);
    key[32..].copy_from_slice(&initial_shared_version.to_be_bytes());
    key
}

/// The epoch's consensus output: what the commit handler decided, written
/// by it alone, as this writer is made once, with the store, and cannot be
/// cloned. Read back only by the commit handler too.
///
/// A quarantine, as the reference's `ConsensusOutputQuarantine`: each
/// commit's output stays in memory until the checkpoints covering it have
/// committed, and is written then, so the store never holds output a crash
/// could make the replay decide otherwise. Reads look in memory, then the
/// store.
pub struct ConsensusOutput {
    quarantine: Arc<Mutex<Quarantine>>,
}

/// Tells the quarantine which checkpoints have committed: for the
/// checkpoint committer.
#[derive(Clone)]
pub struct CommittedCheckpoints {
    quarantine: Arc<Mutex<Quarantine>>,
}

struct Quarantine {
    db: Arc<EpochDb>,
    /// Every transaction deferred and not yet taken, as written and queued:
    /// few, so all in memory, as the reference's `ConsensusOutputCache`.
    deferred: BTreeMap<DeferralKey, Vec<u8>>,
    /// The commit being decided.
    open: Output,
    /// Decided commits not yet written, in commit order.
    queued: std::collections::VecDeque<Output>,
    /// The latest queued value of each key, and how many queued outputs
    /// wrote it: a key leaves memory with the last of them, as the
    /// reference's `RefCountedHashMap`.
    processed: HeapMessageMap<TxDigest, usize>,
    locks: SipMap<ObjectRef, (TxDigest, usize)>,
    streams: SipMap<StreamId, ((u32, u32), usize)>,
    /// The last chunk height of the last checkpoint committed.
    committed_height: u64,
}

/// A table keyed by object references or ids from transactions' inputs,
/// which a client chooses: hashed whole, by `SipHash`.
type SipMap<K, V> = HashMap<K, V, std::hash::RandomState>;

/// One commit's output.
struct Output {
    processed: HeapMessageSet<TxDigest>,
    locks: SipMap<ObjectRef, TxDigest>,
    streams: SipMap<StreamId, (u32, u32)>,
    /// The execution time observations taken: (generation, authority
    /// index) and the encoded estimates.
    observations: Vec<((u64, u32), Vec<u8>)>,
    /// Transactions deferred, encoded, and the keys taken, in order.
    deferred: Vec<(DeferralKey, Option<Vec<u8>>)>,
    /// The JSON web key votes counted, encoded.
    jwk_votes: Vec<Vec<u8>>,
    /// The keys that became active, encoded, if the epoch agrees keys: the
    /// next commit's update.
    new_jwks: Option<Vec<u8>>,
    /// Capabilities recorded, by authority index, encoded.
    capabilities: Vec<(u32, Vec<u8>)>,
    /// The commit stage's state after the commit, encoded.
    record: Vec<u8>,
    /// The last chunk height the commit assigned.
    height: u64,
    /// Whether no chunk waited for a checkpoint after the commit.
    drained: bool,
}

impl Default for Output {
    fn default() -> Output {
        Output {
            processed: containers::heap_message_set(0),
            locks: SipMap::default(),
            streams: SipMap::default(),
            observations: Vec::new(),
            deferred: Vec::new(),
            jwk_votes: Vec::new(),
            new_jwks: None,
            capabilities: Vec::new(),
            record: Vec::new(),
            height: 0,
            drained: false,
        }
    }
}

fn lock_quarantine(quarantine: &Mutex<Quarantine>) -> std::sync::MutexGuard<'_, Quarantine> {
    quarantine
        .lock()
        .expect("the quarantine's lock is not poisoned")
}

impl ConsensusOutput {
    /// Whether the user transaction with `digest` was processed this epoch.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn is_processed(&self, digest: &TxDigest) -> bool {
        let q = lock_quarantine(&self.quarantine);
        q.open.processed.contains(digest)
            || q.processed.contains_key(digest)
            || q.db
                .exists(Space::Processed, &digest.bytes)
                .expect("the epoch's database reads")
    }

    pub fn set_processed(&mut self, digest: TxDigest) {
        lock_quarantine(&self.quarantine)
            .open
            .processed
            .insert(digest);
    }

    /// The transaction holding owned reference `object`'s lock, if any.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn lock_holder(&self, object: &ObjectRef) -> Option<TxDigest> {
        let q = lock_quarantine(&self.quarantine);
        if let Some(holder) = q.open.locks.get(object) {
            return Some(*holder);
        }
        if let Some((holder, _)) = q.locks.get(object) {
            return Some(*holder);
        }
        q.db.get(Space::Locks, &lock_key(object))
            .expect("the epoch's database reads")
            // This handler wrote it, from a transaction's computed digest.
            .map(|v| Hashed::computed(Digest::new(v[..].try_into().expect("a 32-byte digest"))))
    }

    pub fn set_lock(&mut self, object: ObjectRef, holder: TxDigest) {
        lock_quarantine(&self.quarantine)
            .open
            .locks
            .insert(object, holder);
    }

    /// The stream's last scheduled generation, (round, generation), if a
    /// round scheduled it this epoch.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn stream(&self, stream: &StreamId) -> Option<(u32, u32)> {
        let q = lock_quarantine(&self.quarantine);
        if let Some(last) = q.open.streams.get(stream) {
            return Some(*last);
        }
        if let Some((last, _)) = q.streams.get(stream) {
            return Some(*last);
        }
        q.db.get(
            Space::Streams,
            &stream_key(&stream.id, stream.initial_shared_version),
        )
        .expect("the epoch's database reads")
        .map(|v| {
            let word = |i: usize| u32::from_le_bytes(v[i..i + 4].try_into().expect("4 bytes"));
            (word(0), word(4))
        })
    }

    pub fn set_stream(&mut self, stream: StreamId, last: (u32, u32)) {
        lock_quarantine(&self.quarantine)
            .open
            .streams
            .insert(stream, last);
    }

    /// Authority `author`'s execution time observations of `generation`,
    /// `estimates` encoded, taken by the commit being decided.
    pub fn put_execution_time_observations(
        &mut self,
        author: u32,
        generation: u64,
        estimates: Vec<u8>,
    ) {
        lock_quarantine(&self.quarantine)
            .open
            .observations
            .push(((generation, author), estimates));
    }

    /// Defers transactions, `transactions` encoded, by `key`: one key per
    /// commit, as the reference's.
    pub fn defer(&mut self, key: DeferralKey, transactions: Vec<u8>) {
        let mut q = lock_quarantine(&self.quarantine);
        let previous = q.deferred.insert(key, transactions.clone());
        assert!(previous.is_none(), "{key:?} deferred to twice");
        q.open.deferred.push((key, Some(transactions)));
    }

    /// Takes the transactions deferred by keys in `keys`, in key order.
    pub fn take_deferred(
        &mut self,
        keys: std::ops::Range<DeferralKey>,
    ) -> Vec<(DeferralKey, Vec<u8>)> {
        let mut q = lock_quarantine(&self.quarantine);
        let taken: Vec<DeferralKey> = q.deferred.range(keys).map(|(key, _)| *key).collect();
        taken
            .into_iter()
            .map(|key| {
                q.open.deferred.push((key, None));
                (key, q.deferred.remove(&key).expect("a key in range"))
            })
            .collect()
    }

    /// Whether any deferred transaction is still waiting, the commit being
    /// decided's included.
    pub fn has_deferred(&self) -> bool {
        !lock_quarantine(&self.quarantine).deferred.is_empty()
    }

    /// Drops every deferred transaction still waiting: the commit being
    /// decided's are not written, earlier ones are deleted.
    pub fn abandon_deferred(&mut self) {
        let mut q = lock_quarantine(&self.quarantine);
        let keys: Vec<DeferralKey> = std::mem::take(&mut q.deferred).into_keys().collect();
        for key in keys {
            match q
                .open
                .deferred
                .iter()
                .position(|(k, put)| *k == key && put.is_some())
            {
                Some(i) => {
                    q.open.deferred.remove(i);
                }
                None => q.open.deferred.push((key, None)),
            }
        }
    }

    /// A JSON web key vote the commit being decided counted, encoded.
    pub fn put_jwk_vote(&mut self, vote: Vec<u8>) {
        lock_quarantine(&self.quarantine).open.jwk_votes.push(vote);
    }

    /// Capabilities the commit being decided recorded for the authority at
    /// committee index `author`, encoded: they replace any kept.
    pub fn put_capabilities(&mut self, author: u32, capabilities: Vec<u8>) {
        lock_quarantine(&self.quarantine)
            .open
            .capabilities
            .push((author, capabilities));
    }

    /// The capabilities kept of the commits written, by authority index:
    /// for the commit handler's start.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn capabilities(&self) -> Vec<Vec<u8>> {
        lock_quarantine(&self.quarantine)
            .db
            .entries(Space::Capabilities)
            .expect("the epoch's database reads")
            .into_iter()
            .map(|(_, capabilities)| capabilities)
            .collect()
    }

    /// The JSON web keys that became active in the commit being decided,
    /// encoded.
    pub fn set_new_jwks(&mut self, keys: Vec<u8>) {
        lock_quarantine(&self.quarantine).open.new_jwks = Some(keys);
    }

    /// The JSON web key votes of the commits written, in no order: for the
    /// commit handler's start.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn jwk_votes(&self) -> Vec<Vec<u8>> {
        lock_quarantine(&self.quarantine)
            .db
            .entries(Space::JwkVotes)
            .expect("the epoch's database reads")
            .into_iter()
            .map(|(_, vote)| vote)
            .collect()
    }

    /// The JSON web keys that became active in the last commit written, as
    /// `set_new_jwks` took them.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn new_jwks(&self) -> Option<Vec<u8>> {
        lock_quarantine(&self.quarantine)
            .db
            .get(Space::Commit, &[1])
            .expect("the epoch's database reads")
    }

    /// The execution time observations of the commits written, by
    /// (generation, authority index): for the commit stage's start, which
    /// replays later commits.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn execution_time_observations(&self) -> Vec<(u64, u32, Vec<u8>)> {
        lock_quarantine(&self.quarantine)
            .db
            .entries(Space::ExecutionTimeObservations)
            .expect("the epoch's database reads")
            .into_iter()
            .map(|(key, estimates)| {
                let generation = u64::from_be_bytes(key[..8].try_into().expect("8 bytes"));
                let author = u32::from_be_bytes(key[8..].try_into().expect("4 bytes"));
                (generation, author, estimates)
            })
            .collect()
    }

    /// Ends the commit being decided: `record` is the commit stage's state
    /// after it, `height` the last chunk height it assigned, and `drained`
    /// whether no chunk waits for a checkpoint after it. Written once the
    /// checkpoints through `height` have committed, and a drained commit at
    /// or after it can be written too.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn finish_commit(&mut self, record: Vec<u8>, height: u64, drained: bool) {
        let mut q = lock_quarantine(&self.quarantine);
        let mut output = std::mem::take(&mut q.open);
        output.record = record;
        output.height = height;
        output.drained = drained;
        q.queue(output);
        q.flush();
    }

    /// The commit stage's state after the last commit written, as
    /// `finish_commit` took it: where a restart resumes.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn record(&self) -> Option<Vec<u8>> {
        lock_quarantine(&self.quarantine)
            .db
            .get(Space::Commit, &[0])
            .expect("the epoch's database reads")
    }

    /// The handle the checkpoint committer tells of each commit.
    pub fn committed_checkpoints(&self) -> CommittedCheckpoints {
        CommittedCheckpoints {
            quarantine: self.quarantine.clone(),
        }
    }
}

impl CommittedCheckpoints {
    /// The checkpoints through the chunk at `height` have committed: writes
    /// what they cover.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn committed_through(&self, height: u64) {
        let mut q = lock_quarantine(&self.quarantine);
        q.committed_height = q.committed_height.max(height);
        q.flush();
    }
}

impl Quarantine {
    fn queue(&mut self, output: Output) {
        for digest in &output.processed {
            *self.processed.entry(*digest).or_default() += 1;
        }
        for (object, holder) in &output.locks {
            let entry = self.locks.entry(*object).or_insert((*holder, 0));
            *entry = (*holder, entry.1 + 1);
        }
        for (stream, last) in &output.streams {
            let entry = self.streams.entry(*stream).or_insert((*last, 0));
            *entry = (*last, entry.1 + 1);
        }
        self.queued.push_back(output);
    }

    /// Writes the queued outputs whose checkpoints have committed, through
    /// the last of them after which no chunk waited for a checkpoint: a
    /// later one's chunks are in checkpoints only a replay from a drained
    /// commit rebuilds (the reference's `1ad56a6b81`). One batch, with the
    /// last record.
    fn flush(&mut self) {
        let covered = self
            .queued
            .iter()
            .take_while(|o| o.height <= self.committed_height)
            .count();
        let Some(last) = self.queued.range(..covered).rposition(|o| o.drained) else {
            return;
        };
        let mut batch = self.db.batch();
        let mut record = Vec::new();
        let mut new_jwks = None;
        for output in self.queued.drain(..=last) {
            for digest in output.processed {
                batch.put(Space::Processed, digest.bytes.to_vec(), Vec::new());
                release(&mut self.processed, &digest, |count| count);
            }
            for (object, holder) in output.locks {
                batch.put(
                    Space::Locks,
                    lock_key(&object).to_vec(),
                    holder.bytes.to_vec(),
                );
                release(&mut self.locks, &object, |held| &mut held.1);
            }
            for (stream, (round, generation)) in output.streams {
                let mut value = round.to_le_bytes().to_vec();
                value.extend_from_slice(&generation.to_le_bytes());
                batch.put(
                    Space::Streams,
                    stream_key(&stream.id, stream.initial_shared_version).to_vec(),
                    value,
                );
                release(&mut self.streams, &stream, |last| &mut last.1);
            }
            for (key, transactions) in output.deferred {
                let key = key.encode().to_vec();
                match transactions {
                    Some(transactions) => batch.put(Space::Deferred, key, transactions),
                    None => batch.delete(Space::Deferred, key),
                }
            }
            for (author, capabilities) in output.capabilities {
                batch.put(
                    Space::Capabilities,
                    author.to_be_bytes().to_vec(),
                    capabilities,
                );
            }
            for vote in output.jwk_votes {
                let key = Blake2b::<U32>::digest(&vote).to_vec();
                batch.put(Space::JwkVotes, key, vote);
            }
            for ((generation, author), estimates) in output.observations {
                let mut key = generation.to_be_bytes().to_vec();
                key.extend_from_slice(&author.to_be_bytes());
                batch.put(Space::ExecutionTimeObservations, key, estimates);
            }
            record = output.record;
            new_jwks = output.new_jwks;
        }
        batch.put(Space::Commit, vec![0], record);
        if let Some(new_jwks) = new_jwks {
            batch.put(Space::Commit, vec![1], new_jwks);
        }
        batch.commit().expect("the epoch's database writes");
    }
}

/// One fewer queued output holds `key`'s value; it leaves memory with the
/// last.
fn release<K: std::hash::Hash + Eq, V, S: std::hash::BuildHasher>(
    map: &mut HashMap<K, V, S>,
    key: &K,
    count: impl Fn(&mut V) -> &mut usize,
) {
    let value = map.get_mut(key).expect("a queued key is counted");
    let count = count(value);
    *count -= 1;
    if *count == 0 {
        map.remove(key);
    }
}

/// An owned reference's key: its id, big-endian version and digest.
fn lock_key(object: &ObjectRef) -> [u8; 72] {
    let mut key = [0; 72];
    key[..32].copy_from_slice(&object.id.0);
    key[32..40].copy_from_slice(&object.version.get().to_be_bytes());
    key[40..].copy_from_slice(&object.digest.bytes);
    key
}
