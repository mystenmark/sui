// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use containers::Bump;
use workqueue::Processor;

use crate::consensus::commit::randomness_state_update_transaction;
use crate::pipeline::chunk_lock::ChunkLock;
use crate::pipeline::items::{
    Chunk, ExecutableTransaction, GateInput, GatedChunk, ScheduledEntry, SettlementInput,
};

/// Passes chunks from `CommitStage` on in height order, holding a chunk,
/// and every chunk after it, until:
/// - if headed by a randomness state update, its round's random bytes
///   arrive; then it puts the update at the chunk's head;
/// - if the synced scheduler is running the chunk's transactions from its
///   certified checkpoint, the chunk lock no longer holds it.
///
/// A later chunk could not go ahead: funds reservations follow consensus
/// order.
///
/// Inward:
/// - `CommitStage`: each chunk (`GateInput::Chunk`), in height order;
/// - `SignatureCollector`: each round's bytes (`GateInput::Bytes`), durable
///   in the epoch store first;
/// - `SyncedScheduler`: chunks released in the chunk lock
///   (`GateInput::Released`), after it releases them there;
/// - p2p endpoint: which round it waits for (`GateInput::Waiting`).
///
/// Outward:
/// - `VersionResolver`: each chunk's transactions (`Chunk`);
/// - `SettlementScheduler`: each chunk's settlement work
///   (`SettlementInput::Chunk`).
pub struct RandomnessGate {
    held: VecDeque<GatedChunk>,
    /// Bytes arrived for rounds not released yet, by round.
    bytes: BTreeMap<u64, Vec<u8>>,
    /// Shared with the synced scheduler, if any.
    chunk_lock: Option<Arc<ChunkLock>>,
    chunks_out: workqueue::Queue<Chunk>,
    settlements_out: workqueue::Queue<SettlementInput>,
}

impl RandomnessGate {
    /// A gate with the bytes of the rounds `recorded` already.
    pub fn new(
        recorded: Vec<(u64, Vec<u8>)>,
        chunks: workqueue::Queue<Chunk>,
        settlements: workqueue::Queue<SettlementInput>,
    ) -> RandomnessGate {
        RandomnessGate {
            held: VecDeque::new(),
            bytes: recorded.into_iter().collect(),
            chunk_lock: None,
            chunks_out: chunks,
            settlements_out: settlements,
        }
    }

    /// Holds each chunk the synced scheduler runs, through `lock`.
    #[must_use]
    pub fn with_chunk_lock(mut self, lock: Arc<ChunkLock>) -> RandomnessGate {
        self.chunk_lock = Some(lock);
        self
    }

    fn release(&mut self) {
        while let Some(front) = self.held.front() {
            if self
                .chunk_lock
                .as_ref()
                .is_some_and(|lock| lock.held(front.chunk.height))
            {
                return;
            }
            let bytes = match &front.update {
                Some(update) => match self.bytes.remove(&update.round) {
                    Some(bytes) => {
                        // Rounds are released in order: earlier ones' bytes,
                        // recorded before a restart, are not needed.
                        self.bytes = self.bytes.split_off(&update.round);
                        Some(bytes)
                    }
                    None => return,
                },
                None => None,
            };
            let GatedChunk {
                mut chunk,
                mut settlement,
                update,
            } = self.held.pop_front().expect("a front");
            if let (Some(update), Some(bytes)) = (update, bytes) {
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
    type Input = GateInput;
    type Output = ();

    fn process(&mut self, input: GateInput, _: &mut Bump) -> Option<()> {
        match input {
            GateInput::Chunk(chunk) => self.held.push_back(chunk),
            GateInput::Bytes { round, bytes } => {
                self.bytes.insert(round, bytes);
            }
            GateInput::Released => {}
            GateInput::Waiting(answer) => {
                let waiting = self.held.front().and_then(|front| {
                    let update = front.update.as_ref()?;
                    (!self.bytes.contains_key(&update.round))
                        .then_some((update.round, front.chunk.checkpoint_seq))
                });
                let _ = answer.send(waiting);
                return None;
            }
        }
        self.release();
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
    use crate::pipeline::items::ChunkSettlement;
    use crate::pipeline::keys::AccumulatorVersion;

    fn gated(height: u64) -> GatedChunk {
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
                root: SharedAccess {
                    id: ObjectId([0xac; 32]),
                    initial_shared_version: 1,
                    mutability: SharedObjectMutability::Mutable,
                    ordinal: SharedOrdinal {
                        round: 1,
                        writes_before: 0,
                    },
                    base: None,
                },
            },
            update: None,
        }
    }

    #[test]
    fn a_chunk_the_synced_scheduler_runs_waits_until_released() {
        let lock = Arc::new(ChunkLock::new(0, Cut { seq: 0, height: 0 }));
        let (chunks, chunks_out) = workqueue::queue(16);
        let (settlements, _settlements_out) = workqueue::queue(16);
        let mut gate =
            RandomnessGate::new(Vec::new(), chunks, settlements).with_chunk_lock(lock.clone());
        let mut run = |input| gate.process(input, &mut Bump::default());
        let heights = || {
            std::iter::from_fn(|| chunks_out.try_pop())
                .map(|c| c.height)
                .collect::<Vec<_>>()
        };

        // The synced scheduler took chunk 2 before the commit stage claimed
        // it.
        lock.take(2, 2);
        lock.claim(3, Cut { seq: 1, height: 3 });
        for height in 1..=3 {
            run(GateInput::Chunk(gated(height)));
        }
        assert_eq!(heights(), vec![1], "chunk 3 waits behind chunk 2");

        lock.release(2);
        run(GateInput::Released);
        assert_eq!(heights(), vec![2, 3]);
    }
}
