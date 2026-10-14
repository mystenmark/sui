// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The tables availability checks read. Each is shared by its writer and
//! its readers across threads; a writer records an entry before notifying
//! its key, so a reader that finds no entry may wait for the key.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
/// availability check and by `Executor` to resolve exact versions.
#[derive(Default)]
pub struct GenerationTable {
    generations: Mutex<FastMap<(StreamId, u32, u32), Generation>>,
}

impl GenerationTable {
    pub fn new() -> GenerationTable {
        GenerationTable::default()
    }

    /// Records generation `generation` of `stream` in `round`.
    pub fn record(&self, stream: StreamId, round: u32, generation: u32, value: Generation) {
        lock(&self.generations).insert((stream, round, generation), value);
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
            .entry((stream, round, generation))
            .or_insert_with(value)
    }

    /// Generation `generation` of `stream` in `round`, if recorded.
    pub fn get(&self, stream: StreamId, round: u32, generation: u32) -> Option<Generation> {
        lock(&self.generations)
            .get(&(stream, round, generation))
            .copied()
    }
}

/// Settled chunks by height. Written by `SettlementScheduler` (owner: the
/// settlement work), read by the checkpoint waiter's availability check and
/// by `ContentsBuilder`.
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
}

impl BuiltCheckpoint {
    /// As the object store keeps it (`Store::put_checkpoint`).
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
        }
    }
}

/// Checkpoints built and certified, by sequence number. Written by
/// `SummaryBuilder` and `CheckpointAggregator` (owner: the checkpoint work),
/// read by the sequence and summary waiters' availability checks.
#[derive(Default)]
pub struct CheckpointStore {
    built: Mutex<BTreeMap<u64, Arc<BuiltCheckpoint>>>,
    certified: Mutex<BTreeMap<u64, Arc<[u8]>>>,
}

impl CheckpointStore {
    pub fn new() -> CheckpointStore {
        CheckpointStore::default()
    }

    pub fn insert_built(&self, checkpoint: BuiltCheckpoint) {
        lock(&self.built).insert(checkpoint.seq, Arc::new(checkpoint));
    }

    pub fn built(&self, seq: u64) -> Option<Arc<BuiltCheckpoint>> {
        lock(&self.built).get(&seq).cloned()
    }

    /// Records the certified summary (`CertifiedCheckpointSummary` BCS).
    pub fn insert_certified(&self, seq: u64, certified: Arc<[u8]>) {
        lock(&self.certified).insert(seq, certified);
    }

    pub fn certified(&self, seq: u64) -> Option<Arc<[u8]>> {
        lock(&self.certified).get(&seq).cloned()
    }
}
