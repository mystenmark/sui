// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors consensus feeds: voting on blocks, and handling commits.

pub mod cache;
pub mod commit;
pub mod vote;

use std::sync::Arc;

use workqueue::{Queue, Sink, Worker, WorkerHandle};

use cache::ConsensusTxCache;
use commit::{CommitHandler, CommitOutcome, CommitRequest};
use vote::{BlockVoter, VoteRequest};

/// Blocks awaiting a vote beyond which they are refused.
pub const VOTE_QUEUE: usize = 1024;

/// Commits awaiting handling beyond which they are refused.
pub const COMMIT_QUEUE: usize = 1024;

/// The voting and commit processors, each on its own thread, sharing the
/// cache of decoded transactions. Correct in any interleaving: a commit
/// handled before its blocks' votes decodes their transactions again.
pub struct ConsensusProcessors {
    pub votes: Queue<VoteRequest>,
    pub commits: Queue<CommitRequest>,
    pub cache: Arc<ConsensusTxCache>,
    // Dropped last: stop and join the threads.
    _voter: WorkerHandle,
    _committer: WorkerHandle,
}

impl ConsensusProcessors {
    /// The processors over `store`; each commit's outcome goes to
    /// `outcomes`.
    pub fn start(
        store: Arc<store::Store>,
        outcomes: impl Sink<CommitOutcome> + 'static,
    ) -> ConsensusProcessors {
        let cache = Arc::new(ConsensusTxCache::new());
        let (votes, vote_inbox) = workqueue::queue(VOTE_QUEUE);
        let (commits, commit_inbox) = workqueue::queue(COMMIT_QUEUE);
        let voting = {
            let (store, cache) = (store.clone(), cache.clone());
            Worker::new("consensus-votes")
                .run(vote_inbox, move || BlockVoter::new(store, cache), |()| {})
                .spawn()
        };
        let committer = {
            let cache = cache.clone();
            Worker::new("consensus-commits")
                .run(
                    commit_inbox,
                    move || CommitHandler::new(store, cache),
                    outcomes,
                )
                .spawn()
        };
        ConsensusProcessors {
            votes,
            commits,
            cache,
            _voter: voting,
            _committer: committer,
        }
    }
}
