// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Handling a consensus commit, minimally: its accepted user transactions
//! are executed one at a time, in commit order, as `SubmitTransaction`
//! executes them. The reference's ordering by gas price, shared-object
//! version assignment, commit prologue and congestion control are not here
//! yet.

use std::collections::HashSet;
use std::sync::Arc;

use ::consensus::{BlockRef, CommittedSubDag, TransactionIndex};
use messages::Message;
use messages::consensus::ConsensusTransaction;
use workqueue::{Processor, Refusal, Refuse};

use crate::checks::{self, VerifiedTransaction};
use crate::consensus::cache::ConsensusTxCache;
use crate::epoch::EpochState;
use crate::processors::{Outcome, execute_and_commit, executed};

/// A commit to handle, in the epoch it belongs to.
pub struct CommitRequest {
    pub epoch: Arc<EpochState>,
    pub commit: CommittedSubDag,
}

/// A full queue drops the commit; consensus replays commits not handled.
impl Refuse for CommitRequest {
    fn refuse(self, _: Refusal) {}
}

/// What became of each user transaction a commit carried.
#[derive(Debug)]
pub struct CommitOutcome {
    pub commit_index: u32,
    pub transactions: Vec<Committed>,
    /// Transactions taken from the cache, and decoded again.
    pub cache_hits: usize,
    pub cache_misses: usize,
}

/// One user transaction of a commit.
#[derive(Debug)]
pub struct Committed {
    pub block: BlockRef,
    pub index: TransactionIndex,
    pub digest: [u8; 32],
    pub fate: Fate,
}

#[derive(Debug)]
pub enum Fate {
    /// Executed, or failed to.
    Executed(Outcome),
    /// Its inputs failed the checks when its turn came: an owned input was
    /// consumed by a transaction earlier in consensus order, which won.
    Dropped(validation::Error),
    /// Committed before, or executed already.
    Duplicate,
}

/// Handles commits in order, executing their transactions against the
/// store. Its state is in memory for now: after a restart it handles
/// commits from the first again, and transactions executed already are
/// duplicates.
pub struct CommitHandler {
    store: Arc<store::Store>,
    cache: Arc<ConsensusTxCache>,
    /// The index of the last commit handled; 0 before the first.
    last_commit: u32,
    /// The digests of the user transactions committed so far.
    processed: HashSet<[u8; 32]>,
}

impl CommitHandler {
    pub fn new(store: Arc<store::Store>, cache: Arc<ConsensusTxCache>) -> CommitHandler {
        CommitHandler {
            store,
            cache,
            last_commit: 0,
            processed: HashSet::new(),
        }
    }

    /// Handles `commit`, or nothing if it was handled before.
    pub fn handle(&mut self, epoch: &EpochState, commit: CommittedSubDag) -> Option<CommitOutcome> {
        // 1. A commit handled before is skipped.
        if commit.commit_index <= self.last_commit {
            return None;
        }
        self.last_commit = commit.commit_index;

        let mut outcome = CommitOutcome {
            commit_index: commit.commit_index,
            transactions: Vec::new(),
            cache_hits: 0,
            cache_misses: 0,
        };
        let leader_round = commit.leader.round;
        let CommittedSubDag {
            blocks, rejected, ..
        } = commit;
        for block in blocks {
            // 2. The block's decoded transactions, if this validator voted on
            //    it.
            let mut cached = self.cache.take(&block.reference);
            let rejected_here = rejected
                .iter()
                .find(|(b, _)| *b == block.reference)
                .map_or(&[][..], |(_, r)| r);
            for (index, bytes) in block.transactions.into_iter().enumerate() {
                let index = index as TransactionIndex;
                // 3. Skip what a quorum rejected, then take the transaction
                //    from the cache or decode it.
                if rejected_here.contains(&index) {
                    continue;
                }
                let entry = cached
                    .as_mut()
                    .and_then(|entries| entries.get_mut(usize::from(index)))
                    .and_then(Option::take);
                let transaction = if let Some(transaction) = entry {
                    outcome.cache_hits += 1;
                    transaction
                } else {
                    // Not voted on here, rejected here, or not a user
                    // transaction.
                    let Some(transaction) = decode_user_transaction(bytes) else {
                        continue;
                    };
                    outcome.cache_misses += 1;
                    transaction
                };
                let digest = transaction.get().0.digest().bytes;
                let fate = self.execute(epoch, transaction);
                outcome.transactions.push(Committed {
                    block: block.reference,
                    index,
                    digest,
                    fate,
                });
            }
        }

        // 7. Blocks of rounds consensus no longer commits will not be.
        let gc_depth = epoch.config.gc_depth();
        if gc_depth > 0 {
            self.cache
                .evict_through(leader_round.saturating_sub(gc_depth));
        }
        Some(outcome)
    }

    /// Steps 4–6 for one transaction: deduplicate, check its inputs, then
    /// execute and commit it.
    fn execute(&mut self, epoch: &EpochState, transaction: VerifiedTransaction) -> Fate {
        let digest = transaction.get().0.digest().bytes;
        if !self.processed.insert(digest) || executed(&self.store, &digest).is_some() {
            return Fate::Duplicate;
        }
        match checks::check_inputs(epoch, &self.store, transaction) {
            Ok(checked) => Fate::Executed(execute_and_commit(&self.store, epoch, &checked)),
            Err(e) => Fate::Dropped(e),
        }
    }
}

/// A committed transaction that missed the cache, if it is a user
/// transaction.
///
/// # Panics
/// If it does not decode: every committed block passed this validator's
/// voting, which decodes each of its transactions.
fn decode_user_transaction(bytes: Vec<u8>) -> Option<VerifiedTransaction> {
    let transaction = Message::<ConsensusTransaction<'static>>::parse(bytes)
        .map_err(|(e, _)| e)
        .expect("a committed transaction decodes");
    let unchecked = transaction.into_user_transaction()?;
    Some(checks::sequenced_by_consensus(unchecked))
}

impl Processor for CommitHandler {
    type Input = CommitRequest;
    type Output = CommitOutcome;

    fn process(&mut self, request: CommitRequest) -> Option<CommitOutcome> {
        self.handle(&request.epoch, request.commit)
    }
}
