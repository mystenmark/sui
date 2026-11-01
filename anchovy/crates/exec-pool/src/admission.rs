// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Causal admission: which ready transactions the executors may run now,
//! so that execution may block on a value another transaction produces
//! without deadlock (the reference's `causal_admission`, #27840).

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::sync::Arc;

use containers::Bump;
use validator::pipeline::items::{AdmissionInput, Executable};
use validator::pipeline::keys::CausalIndex;
use validator::pipeline::tables::ChunkLengths;
use workqueue::{Processor, Queue};

/// Admits ready transactions to the executors by causal index.
///
/// Execution may block on a value an undeclared dependency produces: the
/// accumulator root a barrier writes. With a bounded number of executors,
/// every one could block on a writer never admitted. So, with C the
/// watermark (every index below it is done), a transaction at index `i` is
/// admitted when fewer than `limit` are in flight, or when `i` is C itself,
/// at most one such at a time: everything it can wait for is done, so it
/// never blocks, and C advances. A transaction at an index already below C
/// ran by the other path; it cannot block. Settlements and barriers have no
/// index: they never block, and run on an executor of their own.
///
/// C crosses from a chunk to the next once the chunk's registered length is
/// done (`ChunkLengths`), which its creator registers before its
/// transactions head here.
///
/// Inward:
/// - input waiter, ready: `AdmissionInput::Ready`;
/// - executors: `AdmissionInput::Retired`, each index they ran.
///
/// Outward:
/// - executors: `Vec<Executable>`, each admitted transaction;
/// - settlement executor: `Vec<Executable>`, each without an index.
pub struct ExecutionAdmission {
    lengths: Arc<ChunkLengths>,
    /// Transactions admitted by the limit at once.
    limit: usize,
    /// C: the first index not done.
    watermark: CausalIndex,
    /// Done at or above C.
    done: BTreeSet<CausalIndex>,
    /// Ready and not admitted, least index first.
    held: BinaryHeap<Reverse<Held>>,
    /// Admitted by the limit and not retired.
    in_flight: usize,
    /// The one admitted past the limit as C, until retired.
    next: Option<CausalIndex>,
    executors: Queue<Vec<Executable>>,
    settlements: Queue<Vec<Executable>>,
}

/// A held transaction, ordered by its index.
struct Held(CausalIndex, Executable);

impl PartialEq for Held {
    fn eq(&self, other: &Held) -> bool {
        self.0 == other.0
    }
}

impl Eq for Held {}

impl PartialOrd for Held {
    fn partial_cmp(&self, other: &Held) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Held {
    fn cmp(&self, other: &Held) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl ExecutionAdmission {
    /// After the chunks through `resumed_height` are done.
    pub fn new(
        lengths: Arc<ChunkLengths>,
        limit: usize,
        resumed_height: u64,
        executors: Queue<Vec<Executable>>,
        settlements: Queue<Vec<Executable>>,
    ) -> ExecutionAdmission {
        ExecutionAdmission {
            lengths,
            limit,
            watermark: CausalIndex {
                height: resumed_height + 1,
                position: 0,
            },
            done: BTreeSet::new(),
            held: BinaryHeap::new(),
            in_flight: 0,
            next: None,
            executors,
            settlements,
        }
    }

    fn retire(&mut self, index: CausalIndex) {
        if self.next == Some(index) {
            self.next = None;
        } else {
            self.in_flight -= 1;
        }
        if index >= self.watermark {
            self.done.insert(index);
        }
        let height = self.watermark.height;
        loop {
            let CausalIndex { height, position } = self.watermark;
            if self.lengths.get(height).is_some_and(|len| position >= len) {
                self.watermark = CausalIndex {
                    height: height + 1,
                    position: 0,
                };
            } else if self.done.remove(&self.watermark) {
                self.watermark.position += 1;
            } else {
                break;
            }
        }
        if self.watermark.height > height {
            self.lengths.prune_below(self.watermark.height);
        }
    }

    fn admit(&mut self) {
        while let Some(Reverse(Held(index, _))) = self.held.peek() {
            let index = *index;
            if index < self.watermark || self.in_flight < self.limit {
                self.in_flight += 1;
            } else if index == self.watermark && self.next.is_none() {
                self.next = Some(index);
            } else {
                return;
            }
            let Reverse(Held(_, executable)) = self.held.pop().expect("peeked");
            self.executors.push(vec![executable], "executor");
        }
    }
}

impl Processor for ExecutionAdmission {
    type Input = AdmissionInput;
    type Output = ();

    fn process(&mut self, input: AdmissionInput, _: &mut Bump) -> Option<()> {
        match input {
            AdmissionInput::Ready(ready) => {
                let mut settlements = Vec::new();
                for executable in ready {
                    match executable.causal {
                        Some(index) => self.held.push(Reverse(Held(index, executable))),
                        None => settlements.push(executable),
                    }
                }
                if !settlements.is_empty() {
                    self.settlements.push(settlements, "settlement executor");
                }
            }
            AdmissionInput::Retired(indices) => {
                for index in indices {
                    self.retire(index);
                }
            }
        }
        self.admit();
        None
    }
}

#[cfg(test)]
mod tests {
    use validator::pipeline::items::{
        ExecutableTransaction, ScheduledEntry, SystemKind, SystemTransaction,
    };
    use validator::pipeline::keys::AccumulatorVersion;

    use super::*;

    fn executable(causal: Option<(u64, u32)>) -> Executable {
        Executable {
            version: AccumulatorVersion(1),
            root_version: 1,
            reserved: Vec::new(),
            entry: ScheduledEntry {
                transaction: ExecutableTransaction::System(SystemTransaction {
                    kind: SystemKind::Barrier { height: 0 },
                    bytes: Vec::new(),
                    digest: messages::base::Hashed::of("TransactionData", &[]),
                }),
                shared: Vec::new(),
                origin: None,
                aliases: None,
                cancelled: Vec::new(),
            },
            inputs: Vec::new(),
            synced: None,
            causal: causal.map(|(height, position)| CausalIndex { height, position }),
        }
    }

    #[test]
    fn the_watermark_s_transaction_runs_past_the_limit_and_crosses_chunks() {
        let lengths = Arc::new(ChunkLengths::default());
        lengths.register(1, 2);
        lengths.register(2, 1);
        let (executors, admitted) = workqueue::queue(16);
        let (settlements, settled) = workqueue::queue(16);
        let mut admission = ExecutionAdmission::new(lengths, 1, 0, executors, settlements);
        let run = |admission: &mut ExecutionAdmission, input| {
            admission.process(input, &mut Bump::default());
        };
        let admitted = || {
            std::iter::from_fn(|| admitted.try_pop())
                .flatten()
                .map(|e| e.causal.map(|c| (c.height, c.position)))
                .collect::<Vec<_>>()
        };

        // A later chunk's transaction takes the one slot, as if it blocked.
        run(
            &mut admission,
            AdmissionInput::Ready(vec![executable(Some((2, 0))), executable(None)]),
        );
        assert_eq!(admitted(), vec![Some((2, 0))]);
        assert_eq!(
            settled.try_pop().map(|s| s.len()),
            Some(1),
            "settlements bypass"
        );

        // The first chunk still runs, one at a time, as the watermark.
        run(
            &mut admission,
            AdmissionInput::Ready(vec![executable(Some((1, 1))), executable(Some((1, 0)))]),
        );
        assert_eq!(admitted(), vec![Some((1, 0))]);
        run(
            &mut admission,
            AdmissionInput::Retired(vec![CausalIndex {
                height: 1,
                position: 0,
            }]),
        );
        assert_eq!(admitted(), vec![Some((1, 1))]);
        run(
            &mut admission,
            AdmissionInput::Retired(vec![CausalIndex {
                height: 1,
                position: 1,
            }]),
        );
        assert_eq!(
            admission.watermark,
            CausalIndex {
                height: 2,
                position: 0
            }
        );

        // Once the first chunk's length is done, the next chunk's first
        // retires the watermark past it.
        run(
            &mut admission,
            AdmissionInput::Retired(vec![CausalIndex {
                height: 2,
                position: 0,
            }]),
        );
        assert_eq!(
            admission.watermark,
            CausalIndex {
                height: 3,
                position: 0
            }
        );
        assert_eq!(admission.in_flight, 0);
    }
}
