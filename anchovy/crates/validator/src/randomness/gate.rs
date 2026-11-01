// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::Arc;

use containers::Bump;
use waiter::Availability;
use workqueue::Processor;

use crate::consensus::commit::randomness_state_update_transaction;
use crate::epoch_store::EpochStore;
use crate::pipeline::chunk_lock::ChunkLock;
use crate::pipeline::items::{
    Chunk, ExecutableTransaction, GatedChunk, ScheduledEntry, SettlementInput,
};
use crate::pipeline::keys::GateKey;

/// The gate waiter's view of its keys: the chunk lock, and the random bytes
/// the epoch store records.
pub struct GateAvailability {
    pub chunk_lock: Arc<ChunkLock>,
    pub epoch_store: EpochStore,
}

impl Availability<GateKey, Option<Vec<u8>>> for GateAvailability {
    fn check(&mut self, keys: &[GateKey], found: &mut Vec<Option<Option<Vec<u8>>>>) {
        found.clear();
        found.extend(keys.iter().map(|key| match *key {
            GateKey::ChunkFree(height) => (!self.chunk_lock.held(height)).then_some(None),
            GateKey::RoundBytes(round) => self.epoch_store.round_bytes(round).map(Some),
        }));
    }
}

/// Passes chunks the gate waiter released on in height order, with a
/// randomness state update put at its chunk's head. A later chunk could not
/// go ahead: funds reservations follow consensus order.
///
/// Inward: gate waiter, ready: `Vec<GatedChunk>`.
///
/// Outward:
/// - `VersionResolver`: each chunk's transactions (`Chunk`);
/// - `SettlementScheduler`: each chunk's settlement work
///   (`SettlementInput::Chunk`).
pub struct RandomnessGate {
    /// Released chunks not passed on yet, by height.
    released: BTreeMap<u64, GatedChunk>,
    /// The height of the next chunk to pass on.
    next: u64,
    chunks_out: workqueue::Queue<Chunk>,
    settlements_out: workqueue::Queue<SettlementInput>,
}

impl RandomnessGate {
    /// A gate whose first chunk is at height `next`.
    pub fn new(
        next: u64,
        chunks: workqueue::Queue<Chunk>,
        settlements: workqueue::Queue<SettlementInput>,
    ) -> RandomnessGate {
        RandomnessGate {
            released: BTreeMap::new(),
            next,
            chunks_out: chunks,
            settlements_out: settlements,
        }
    }
}

/// Inside the pipeline a full queue is a sizing bug; a closed one means the
/// pipeline is shutting down.
fn push<T>(queue: &workqueue::Queue<T>, item: T, name: &str) {
    if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
        panic!("the {name} queue refused work");
    }
}

impl Processor for RandomnessGate {
    type Input = Vec<GatedChunk>;
    type Output = ();

    fn process(&mut self, released: Vec<GatedChunk>, _: &mut Bump) -> Option<()> {
        for gated in released {
            self.released.insert(gated.chunk.height, gated);
        }
        while let Some(gated) = self.released.remove(&self.next) {
            self.next += 1;
            let GatedChunk {
                mut chunk,
                mut settlement,
                update,
                bytes,
            } = gated;
            if let Some(update) = update {
                let bytes = bytes.expect("released with its round's bytes");
                let system = randomness_state_update_transaction(&update, bytes);
                settlement.digests.insert(0, system.digest);
                chunk.entries.insert(
                    0,
                    ScheduledEntry {
                        transaction: ExecutableTransaction::System(system),
                        shared: vec![update.shared],
                        origin: None,
                        cancelled: Vec::new(),
                    },
                );
            }
            push(&self.chunks_out, chunk, "resolver");
            push(
                &self.settlements_out,
                SettlementInput::Chunk(settlement),
                "settlement",
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use messages::base::ObjectId;
    use messages::transaction::SharedObjectMutability;

    use super::*;
    use crate::consensus::sequence::{SharedAccess, SharedOrdinal};
    use crate::pipeline::chunk_lock::Cut;
    use crate::pipeline::items::{ChunkSettlement, PendingUpdate};
    use crate::pipeline::keys::AccumulatorVersion;

    fn gated(height: u64, update: Option<PendingUpdate>) -> GatedChunk {
        let root = SharedAccess {
            id: ObjectId([0xac; 32]),
            initial_shared_version: 1,
            mutability: SharedObjectMutability::Mutable,
            ordinal: SharedOrdinal {
                round: 1,
                writes_before: 0,
            },
            base: None,
        };
        GatedChunk {
            chunk: Chunk {
                version: AccumulatorVersion(1),
                root_version: 1,
                round: 1,
                height,
                checkpoint_seq: 1,
                entries: Vec::new(),
            },
            settlement: ChunkSettlement {
                version: AccumulatorVersion(1),
                root_version: 1,
                last_of_commit: true,
                round: 1,
                height,
                checkpoint_seq: 1,
                digests: Vec::new(),
                root,
            },
            update,
            bytes: None,
        }
    }

    #[test]
    fn a_chunk_waits_for_the_synced_scheduler_and_its_round_and_holds_those_after() {
        let dir = tempfile::tempdir().unwrap();
        let (epoch_store, _, _) = EpochStore::open(dir.path(), 0);
        let lock = Arc::new(ChunkLock::new(0, Cut { seq: 0, height: 0 }));
        let mut waits = waiter::Waiter::new(GateAvailability {
            chunk_lock: lock.clone(),
            epoch_store: epoch_store.clone(),
        });
        let (chunks, chunks_out) = workqueue::queue(16);
        let (settlements, _settlements_out) = workqueue::queue(16);
        let mut gate = RandomnessGate::new(1, chunks, settlements);
        let mut run = |waits: &mut waiter::Waiter<_, _, _, _>| {
            let mut ready = Vec::new();
            waits.get_ready(&mut ready);
            gate.process(ready, &mut Bump::default());
            std::iter::from_fn(|| chunks_out.try_pop())
                .map(|c| (c.height, c.entries.len()))
                .collect::<Vec<_>>()
        };

        // The synced scheduler took chunk 2 before the commit stage claimed
        // it; chunk 3 is headed by round 7's update.
        lock.take(2, 2);
        lock.claim(4, Cut { seq: 1, height: 4 });
        let update = PendingUpdate {
            epoch: 0,
            round: 7,
            initial_shared_version: 1,
            shared: gated(0, None).settlement.root,
        };
        let mut batch = waiter::WaitBatch::new();
        for height in 1..=4 {
            let update = (height == 3).then_some(update);
            let keys = std::iter::once(GateKey::ChunkFree(height))
                .chain(update.map(|u| GateKey::RoundBytes(u.round)));
            batch.push(gated(height, update), keys);
        }
        waits.wait_for(&mut batch);
        assert_eq!(
            run(&mut waits),
            vec![(1, 0)],
            "the rest wait behind chunk 2"
        );

        lock.release(2);
        waits.resolve([(GateKey::ChunkFree(2), None)]);
        assert_eq!(
            run(&mut waits),
            vec![(2, 0)],
            "chunk 4 waits behind round 7"
        );

        epoch_store.put_randomness_bytes(7, &[1; 96]);
        waits.resolve([(GateKey::RoundBytes(7), Some(vec![1; 96]))]);
        assert_eq!(run(&mut waits), vec![(3, 1), (4, 0)]);
    }
}
