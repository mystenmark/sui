// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Holding consensus back while too much it committed is not yet in a
//! committed checkpoint, as the reference's `BackpressureManager`: the
//! commit stage's answer to a commit, which consensus awaits before sending
//! the next, is withheld until the checkpoint committer catches up.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

/// How much may be in flight: scheduled by the commit stage, and not yet in
/// a committed checkpoint.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub chunks: usize,
    pub transactions: usize,
}

pub struct Backpressure {
    limits: Limits,
    state: Mutex<State>,
}

struct State {
    /// The transactions of each chunk in flight, by height.
    chunks: BTreeMap<u64, usize>,
    transactions: usize,
    certified: u64,
    committed: u64,
    withheld: Vec<oneshot::Sender<()>>,
}

impl State {
    /// Over a limit, unless every certified checkpoint has committed: then
    /// only more commits can certify more, and holding them back would
    /// deadlock.
    fn pressured(&self, limits: &Limits) -> bool {
        self.certified > self.committed
            && (self.chunks.len() > limits.chunks || self.transactions > limits.transactions)
    }
}

impl Backpressure {
    /// Nothing in flight, checkpoints certified and committed through
    /// `committed`.
    pub fn new(limits: Limits, committed: u64) -> Backpressure {
        Backpressure {
            limits,
            state: Mutex::new(State {
                chunks: BTreeMap::new(),
                transactions: 0,
                certified: committed,
                committed,
                withheld: Vec::new(),
            }),
        }
    }

    /// The chunk at `height`, of `transactions`, is scheduled.
    pub fn scheduled(&self, height: u64, transactions: usize) {
        let mut state = self.lock();
        state.transactions += transactions;
        let previous = state.chunks.insert(height, transactions);
        assert!(previous.is_none(), "chunk {height} scheduled twice");
    }

    /// Sends `handled` now, or once the pressure lifts.
    pub fn answer(&self, handled: oneshot::Sender<()>) {
        let mut state = self.lock();
        if state.pressured(&self.limits) {
            state.withheld.push(handled);
        } else {
            // Consensus may have stopped waiting; nothing to do then.
            let _ = handled.send(());
        }
    }

    /// Checkpoint `seq` is certified.
    pub fn certified(&self, seq: u64) {
        let mut state = self.lock();
        state.certified = state.certified.max(seq);
    }

    /// Checkpoint `seq`, whose last chunk is at `last_height`, has
    /// committed: the chunks through it are no longer in flight.
    pub fn committed(&self, seq: u64, last_height: u64) {
        let mut state = self.lock();
        state.committed = state.committed.max(seq);
        let in_flight = state.chunks.split_off(&(last_height + 1));
        let done = std::mem::replace(&mut state.chunks, in_flight);
        state.transactions -= done.values().sum::<usize>();
        if !state.pressured(&self.limits) {
            for handled in state.withheld.drain(..) {
                let _ = handled.send(());
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every critical section leaves the state consistent before it can
        // panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        chunks: 2,
        transactions: 10,
    };

    fn answered(backpressure: &Backpressure) -> oneshot::Receiver<()> {
        let (handled, answer) = oneshot::channel();
        backpressure.answer(handled);
        answer
    }

    #[test]
    fn answers_at_once_under_the_limits() {
        let backpressure = Backpressure::new(LIMITS, 0);
        backpressure.scheduled(1, 10);
        backpressure.scheduled(2, 0);
        backpressure.certified(1);
        assert_eq!(answered(&backpressure).try_recv(), Ok(()));
    }

    #[test]
    fn withholds_over_the_chunk_limit_until_a_checkpoint_commits() {
        let backpressure = Backpressure::new(LIMITS, 0);
        for height in 1..=3 {
            backpressure.scheduled(height, 1);
        }
        backpressure.certified(1);
        let mut answer = answered(&backpressure);
        assert!(answer.try_recv().is_err());
        backpressure.committed(1, 2);
        assert_eq!(answer.try_recv(), Ok(()));
    }

    #[test]
    fn withholds_over_the_transaction_limit_until_it_is_under() {
        let backpressure = Backpressure::new(LIMITS, 0);
        backpressure.scheduled(1, 6);
        backpressure.scheduled(2, 6);
        backpressure.certified(3);
        let mut answer = answered(&backpressure);
        assert!(answer.try_recv().is_err());
        // A checkpoint of none of them.
        backpressure.committed(1, 0);
        assert!(answer.try_recv().is_err());
        backpressure.committed(2, 1);
        assert_eq!(answer.try_recv(), Ok(()));
    }

    #[test]
    fn never_withholds_with_every_certified_checkpoint_committed() {
        let backpressure = Backpressure::new(LIMITS, 5);
        for height in 1..=10 {
            backpressure.scheduled(height, 100);
        }
        assert_eq!(answered(&backpressure).try_recv(), Ok(()));
        backpressure.certified(6);
        let mut answer = answered(&backpressure);
        assert!(answer.try_recv().is_err());
        // Committing what was certified lifts it, though still over.
        backpressure.committed(6, 1);
        assert_eq!(answer.try_recv(), Ok(()));
    }
}
