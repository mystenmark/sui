// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, VecDeque};

use containers::Bump;
use workqueue::Processor;

use crate::consensus::commit::randomness_state_update_transaction;
use crate::pipeline::items::{
    Chunk, ExecutableTransaction, GateInput, GatedChunk, ScheduledEntry, SettlementInput,
};

/// Passes chunks from `CommitStage` on in height order, holding a chunk
/// headed by a randomness state update, and every chunk after it, until its
/// round's random bytes arrive; then puts the update at the chunk's head.
/// A later chunk could not go ahead: funds reservations follow consensus
/// order.
///
/// Inward:
/// - `CommitStage`: each chunk (`GateInput::Chunk`), in height order;
/// - `SignatureCollector`: each round's bytes (`GateInput::Bytes`), durable
///   in the epoch store first;
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
            chunks_out: chunks,
            settlements_out: settlements,
        }
    }

    fn release(&mut self) {
        while let Some(front) = self.held.front() {
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
            GateInput::Waiting(answer) => {
                let waiting = self.held.front().and_then(|front| {
                    let update = front.update.as_ref()?;
                    Some((update.round, front.chunk.checkpoint_seq))
                });
                let _ = answer.send(waiting);
                return None;
            }
        }
        self.release();
        None
    }
}
