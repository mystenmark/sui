// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The tables availability checks read. Each is shared by its writer and
//! its readers across threads; a writer records an entry before notifying
//! its key, so a reader that finds no entry may wait for the key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::epoch_store::{BuilderOutput, EpochStore};
use crate::pipeline::items::SettledChunk;
use crate::pipeline::keys::StreamId;

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

/// Stream generations by `(stream, round, generation)`, in the epoch
/// store. Written by `Executor` (owner: the exec-pool work), read by the
/// input waiter's availability check and by `Executor` to resolve exact
/// versions.
pub struct GenerationTable {
    store: EpochStore,
    /// Makes `get_or_record`'s read and write one step.
    recording: Mutex<()>,
}

impl GenerationTable {
    pub fn new(store: EpochStore) -> GenerationTable {
        GenerationTable {
            store,
            recording: Mutex::new(()),
        }
    }

    /// Records generation `generation` of `stream` in `round`.
    pub fn record(&self, stream: StreamId, round: u32, generation: u32, value: Generation) {
        self.store.put_generation(&stream, round, generation, value);
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
        let _recording = lock(&self.recording);
        if let Some(recorded) = self.store.generation(&stream, round, generation) {
            return recorded;
        }
        let value = value();
        self.store.put_generation(&stream, round, generation, value);
        value
    }

    /// Generation `generation` of `stream` in `round`, if recorded.
    pub fn get(&self, stream: StreamId, round: u32, generation: u32) -> Option<Generation> {
        self.store.generation(&stream, round, generation)
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

/// Checkpoints built, by sequence number, in the epoch store's checkpoint
/// builder state, with the previous epoch's last. Written by
/// `SummaryBuilder` (owner: the checkpoint work); read by the built
/// waiter's availability check, the aggregator and the committer.
pub struct CheckpointStore {
    store: EpochStore,
    output: BuilderOutput,
}

impl CheckpointStore {
    pub fn new(store: EpochStore, output: BuilderOutput) -> CheckpointStore {
        CheckpointStore { store, output }
    }

    pub fn insert_built(&self, checkpoint: BuiltCheckpoint) -> Arc<BuiltCheckpoint> {
        self.output.put_built(checkpoint.seq, checkpoint.encode());
        Arc::new(checkpoint)
    }

    pub fn built(&self, seq: u64) -> Option<Arc<BuiltCheckpoint>> {
        self.store
            .built(seq)
            .map(|bytes| Arc::new(BuiltCheckpoint::decode(&bytes)))
    }
}
