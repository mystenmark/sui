// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The tables availability checks read. Each is shared by its writer and
//! its readers across threads; a writer records an entry before notifying
//! its key, so a reader that finds no entry may wait for the key.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::epoch_store::EpochStore;
use crate::pipeline::items::SettledChunk;
use crate::pipeline::keys::StreamId;

type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every critical section here is a single map operation, which leaves
    // the map consistent even if it panicked.
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
/// availability check and by `Executor` to resolve exact versions, pruned by
/// `CheckpointCommitter` (`prune_below`).
pub struct GenerationTable {
    generations: Mutex<Generations>,
    /// Where pruned generations that readers may still ask for are kept.
    store: EpochStore,
}

#[derive(Default)]
struct Generations {
    recorded: FastMap<(StreamId, u32, u32), Generation>,
    /// Rounds below this are pruned: their generations are in the epoch
    /// store, if a reader may still ask for them.
    pruned_below: u32,
}

impl GenerationTable {
    pub fn new(store: EpochStore) -> GenerationTable {
        GenerationTable {
            generations: Mutex::default(),
            store,
        }
    }

    /// Records generation `generation` of `stream` in `round`.
    pub fn record(&self, stream: StreamId, round: u32, generation: u32, value: Generation) {
        lock(&self.generations)
            .recorded
            .insert((stream, round, generation), value);
    }

    /// Generation `generation` of `stream` in `round`: the one recorded, or
    /// else `value()`, recorded now. Atomic, so concurrent callers agree.
    pub fn get_or_record(
        &self,
        stream: StreamId,
        round: u32,
        generation: u32,
        value: impl FnOnce() -> Generation,
    ) -> Generation {
        *lock(&self.generations)
            .recorded
            .entry((stream, round, generation))
            .or_insert_with(value)
    }

    /// Generation `generation` of `stream` in `round`, if recorded.
    ///
    /// # Panics
    /// If it was pruned without being kept: no reader asks for one.
    pub fn get(&self, stream: StreamId, round: u32, generation: u32) -> Option<Generation> {
        let generations = lock(&self.generations);
        if let Some(found) = generations.recorded.get(&(stream, round, generation)) {
            return Some(*found);
        }
        if round >= generations.pruned_below {
            return None;
        }
        let kept = self.store.generation(&stream, round, generation);
        assert!(
            kept.is_some(),
            "generation {generation} of {stream:?} in round {round} was pruned, not kept"
        );
        kept
    }

    /// Prunes the rounds below `round`, all of whose transactions have
    /// executed. Each stream's last generation among them stays readable,
    /// from the epoch store: a later round reads it until the stream is
    /// written again. No other is read again.
    pub fn prune_below(&self, round: u32) {
        let mut generations = lock(&self.generations);
        if round <= generations.pruned_below {
            return;
        }
        let mut last: FastMap<StreamId, (u32, u32, Generation)> = FastMap::default();
        for (&(stream, r, k), value) in &generations.recorded {
            if r < round
                && last
                    .get(&stream)
                    .is_none_or(|&(lr, lk, _)| (r, k) > (lr, lk))
            {
                last.insert(stream, (r, k, *value));
            }
        }
        let kept: Vec<_> = last
            .into_iter()
            .map(|(stream, (r, k, value))| (stream, r, k, value))
            .collect();
        // Written before they leave memory, under the lock: a reader never
        // finds them in neither.
        self.store.put_generations(&kept);
        generations.recorded.retain(|&(_, r, _), _| r >= round);
        generations.pruned_below = round;
    }
}

/// Settled chunks by height. Written by `SettlementScheduler` (owner: the
/// settlement work), read by the checkpoint waiter's availability check and
/// by `ContentsBuilder`, pruned by `CheckpointCommitter`.
#[derive(Default)]
pub struct ChunkTable {
    chunks: Mutex<FastMap<u64, Arc<SettledChunk>>>,
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
/// (owner: the checkpoint work) and pruned by `CheckpointCommitter`; read by
/// the built waiter's availability check.
#[derive(Default)]
pub struct CheckpointStore {
    built: Mutex<Built>,
}

#[derive(Default)]
struct Built {
    checkpoints: BTreeMap<u64, Arc<BuiltCheckpoint>>,
    /// Every checkpoint below was built, and pruned.
    pruned_below: u64,
}

impl CheckpointStore {
    pub fn new() -> CheckpointStore {
        CheckpointStore::default()
    }

    pub fn insert_built(&self, checkpoint: BuiltCheckpoint) -> Arc<BuiltCheckpoint> {
        let checkpoint = Arc::new(checkpoint);
        lock(&self.built)
            .checkpoints
            .insert(checkpoint.seq, checkpoint.clone());
        checkpoint
    }

    pub fn built(&self, seq: u64) -> Option<Arc<BuiltCheckpoint>> {
        lock(&self.built).checkpoints.get(&seq).cloned()
    }

    /// Whether checkpoint `seq` was built, though perhaps pruned since.
    pub fn is_built(&self, seq: u64) -> bool {
        let built = lock(&self.built);
        seq < built.pruned_below || built.checkpoints.contains_key(&seq)
    }

    /// Drops the checkpoints below `seq`.
    pub fn prune_below(&self, seq: u64) {
        let mut built = lock(&self.built);
        built.checkpoints = built.checkpoints.split_off(&seq);
        built.pruned_below = built.pruned_below.max(seq);
    }
}
