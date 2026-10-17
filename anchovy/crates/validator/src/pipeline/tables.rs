// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The tables availability checks read. Each is shared by its writer and
//! its readers across threads; a writer records an entry before notifying
//! its key, so a reader that finds no entry may wait for the key.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::epoch_store::{BuilderOutput, EpochStore};
use crate::pipeline::items::SettledChunk;
use crate::pipeline::keys::{StreamId, TxDigest};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every critical section here is a single operation, which leaves the
    // data consistent even if it panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One generation of a shared-object stream: the version the generation's
/// writer produced, or the stream's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// The object at this version and digest.
    Version { version: u64, digest: [u8; 32] },
    /// The stream ended: the writer deleted the object, wrapped it or took it
    /// out of consensus, at `version`, in `transaction`.
    Ended { version: u64, transaction: [u8; 32] },
}

/// Stream generations by `(stream, round, generation)`. Written by
/// `Executor` (owner: the exec-pool work), read by the input waiter's
/// availability check and by `Executor` to resolve exact versions.
///
/// A generation names an object a transaction wrote, so it is in memory
/// until that transaction's checkpoint commits, as the object is in the
/// object store's cache; then it is written to the epoch store
/// (`commit`). So after a crash the epoch store names only objects the
/// object store has.
pub struct GenerationTable {
    store: EpochStore,
    /// Makes `get_or_record`'s read and write one step.
    recording: Mutex<()>,
    uncommitted: Mutex<Uncommitted>,
}

type GenerationKey = (StreamId, u32, u32);

#[derive(Default)]
struct Uncommitted {
    generations: HashMap<GenerationKey, Generation, foldhash::fast::RandomState>,
    /// The generations each transaction produced.
    by_producer: HashMap<TxDigest, Vec<GenerationKey>, foldhash::fast::RandomState>,
}

impl GenerationTable {
    pub fn new(store: EpochStore) -> GenerationTable {
        GenerationTable {
            store,
            recording: Mutex::new(()),
            uncommitted: Mutex::default(),
        }
    }

    /// Records generation `generation` of `stream` in `round`, produced by
    /// transaction `producer`; written once `producer` commits, or at once
    /// if it has.
    pub fn record(
        &self,
        stream: StreamId,
        round: u32,
        generation: u32,
        value: Generation,
        producer: TxDigest,
        committed: bool,
    ) {
        if committed {
            self.store
                .put_generations(&[(stream, round, generation, value)]);
            return;
        }
        let mut uncommitted = lock(&self.uncommitted);
        let key = (stream, round, generation);
        uncommitted.generations.insert(key, value);
        uncommitted
            .by_producer
            .entry(producer)
            .or_default()
            .push(key);
    }

    /// Generation `generation` of `stream` in `round`: the one recorded, or
    /// else `value()`, recorded now. Atomic, so concurrent callers agree.
    /// For generations no transaction produced (a stream's start), which
    /// are written at once.
    pub fn get_or_record(
        &self,
        stream: StreamId,
        round: u32,
        generation: u32,
        value: impl FnOnce() -> Generation,
    ) -> Generation {
        let _recording = lock(&self.recording);
        if let Some(recorded) = self.get(stream, round, generation) {
            return recorded;
        }
        let value = value();
        self.store
            .put_generations(&[(stream, round, generation, value)]);
        value
    }

    /// Generation `generation` of `stream` in `round`, if recorded.
    pub fn get(&self, stream: StreamId, round: u32, generation: u32) -> Option<Generation> {
        if let Some(found) = lock(&self.uncommitted)
            .generations
            .get(&(stream, round, generation))
        {
            return Some(*found);
        }
        self.store.generation(&stream, round, generation)
    }

    /// Writes the generations `transactions`, which have committed,
    /// produced.
    pub fn commit(&self, transactions: &[TxDigest]) {
        let mut uncommitted = lock(&self.uncommitted);
        let mut written = Vec::new();
        for digest in transactions {
            for key in uncommitted.by_producer.remove(digest).unwrap_or_default() {
                let value = uncommitted.generations[&key];
                written.push((key.0, key.1, key.2, value));
            }
        }
        // Written before they leave memory, under the lock: a reader never
        // finds them in neither.
        self.store.put_generations(&written);
        for (stream, round, generation, _) in written {
            uncommitted.generations.remove(&(stream, round, generation));
        }
    }
}

/// Settled chunks by height, in memory: only checkpoints not yet committed
/// read them, so they are pruned as checkpoints commit, and a replay
/// settles them again. Written by `SettlementScheduler` (owner: the
/// settlement work), read by the checkpoint waiter's availability check and
/// by `ContentsBuilder`, pruned by `CheckpointCommitter`.
#[derive(Default)]
pub struct ChunkTable {
    chunks: Mutex<HashMap<u64, Arc<SettledChunk>, foldhash::fast::RandomState>>,
}

impl ChunkTable {
    pub fn new() -> ChunkTable {
        ChunkTable::default()
    }

    pub fn insert(&self, chunk: SettledChunk) {
        lock(&self.chunks).insert(chunk.height, Arc::new(chunk));
    }

    pub fn get(&self, height: u64) -> Option<Arc<SettledChunk>> {
        lock(&self.chunks).get(&height).cloned()
    }

    /// Drops the chunks at or below `height`.
    pub fn prune_through(&self, height: u64) {
        lock(&self.chunks).retain(|&h, _| h > height);
    }
}

/// A checkpoint this validator built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltCheckpoint {
    pub seq: u64,
    /// `CheckpointSummary` BCS and digest.
    pub summary: Vec<u8>,
    pub digest: [u8; 32],
    pub contents: Vec<u8>,
    /// For the next checkpoint's summary.
    pub network_total_transactions: u64,
    /// The epoch's rolling gas cost: computation, storage, rebate,
    /// non-refundable.
    pub epoch_rolling_gas: [u64; 4],
    /// Its last chunk's height, and its timestamp: where a restart's chunks
    /// and checkpoint intervals continue from.
    pub last_height: u64,
    pub timestamp_ms: u64,
    /// The epoch's last checkpoint only: the live object set's ECMH after it
    /// (`StateHash` BCS), which the next epoch continues from.
    pub root_state: Option<Vec<u8>>,
}

impl BuiltCheckpoint {
    /// As the object store keeps it (`Store::commit_checkpoint`).
    pub fn encode(&self) -> Vec<u8> {
        bcs::to_bytes(&(
            self.seq,
            &self.summary,
            self.digest,
            &self.contents,
            self.network_total_transactions,
            self.epoch_rolling_gas,
            self.last_height,
            self.timestamp_ms,
            &self.root_state,
        ))
        .expect("a checkpoint encodes")
    }

    /// # Panics
    /// If `bytes` is not what `encode` wrote.
    pub fn decode(bytes: &[u8]) -> BuiltCheckpoint {
        let (
            seq,
            summary,
            digest,
            contents,
            network_total_transactions,
            epoch_rolling_gas,
            last_height,
            timestamp_ms,
            root_state,
        ) = bcs::from_bytes(bytes).expect("a stored checkpoint decodes");
        BuiltCheckpoint {
            seq,
            summary,
            digest,
            contents,
            network_total_transactions,
            epoch_rolling_gas,
            last_height,
            timestamp_ms,
            root_state,
        }
    }
}

/// Checkpoints built, by sequence number. Written by `SummaryBuilder`
/// (owner: the checkpoint work); read by the built waiter's availability
/// check, the aggregator and the committer; pruned by the committer.
///
/// As the reference's builder summaries, a checkpoint is built only once
/// built in this run, held in memory until it commits, or committed, then
/// read from the object store. Every checkpoint built is also kept in the
/// epoch store's builder state, as the reference's
/// `locally_computed_checkpoints`, but only to check one built again after
/// a restart: one built before a crash may hold transactions the crash
/// lost, which must execute again before it can be certified or committed.
pub struct CheckpointStore {
    objects: Arc<store::Store>,
    store: EpochStore,
    output: BuilderOutput,
    /// Built in this run, not yet committed.
    built: Mutex<BTreeMap<u64, Arc<BuiltCheckpoint>>>,
    /// The last checkpoint committed.
    committed: AtomicU64,
}

impl CheckpointStore {
    /// Checkpoints committed through `committed`, in `objects`.
    pub fn new(
        objects: Arc<store::Store>,
        committed: u64,
        store: EpochStore,
        output: BuilderOutput,
    ) -> CheckpointStore {
        CheckpointStore {
            objects,
            store,
            output,
            built: Mutex::default(),
            committed: AtomicU64::new(committed),
        }
    }

    /// Records `checkpoint`, built here.
    ///
    /// # Panics
    /// If a checkpoint of the same sequence number was built with another
    /// digest, in this run or before a restart: a fork, as this validator
    /// may have signed the first.
    pub fn insert_built(&self, checkpoint: BuiltCheckpoint) -> Arc<BuiltCheckpoint> {
        if let Some(bytes) = self.store.built(checkpoint.seq) {
            assert!(
                BuiltCheckpoint::decode(&bytes).digest == checkpoint.digest,
                "fork: checkpoint {} built again differently",
                checkpoint.seq
            );
        }
        self.output.put_built(checkpoint.seq, checkpoint.encode());
        let checkpoint = Arc::new(checkpoint);
        lock(&self.built).insert(checkpoint.seq, checkpoint.clone());
        checkpoint
    }

    /// Checkpoint `seq`, if built in this run or committed.
    ///
    /// # Panics
    /// If the object store cannot be read.
    pub fn built(&self, seq: u64) -> Option<Arc<BuiltCheckpoint>> {
        if let Some(built) = lock(&self.built).get(&seq) {
            return Some(built.clone());
        }
        if seq > self.committed.load(Ordering::Acquire) {
            return None;
        }
        let bytes = self.objects.checkpoint(seq).expect("the store reads")?;
        Some(Arc::new(BuiltCheckpoint::decode(&bytes)))
    }

    /// Checkpoint `seq` has committed: those before it are read from the
    /// object store from now on.
    pub fn committed(&self, seq: u64) {
        self.committed.fetch_max(seq, Ordering::AcqRel);
        let mut built = lock(&self.built);
        *built = built.split_off(&seq);
    }
}
