// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors consensus feeds: voting on blocks, and handling commits.

pub mod cache;
pub mod commit;
pub mod decode;
pub mod sequence;
pub mod status;
pub mod vote;

use std::sync::Arc;

use workqueue::{Queue, Sink, Worker, WorkerHandle};

use cache::ConsensusTxCache;
use commit::{CommitHandler, CommitOutcome, ScheduleExecutor};
use decode::{CommitDecoder, CommitRequest};
use vote::{BlockVoter, VoteRequest};

/// Blocks awaiting a vote beyond which they are refused.
pub const VOTE_QUEUE: usize = 1024;

/// Commits awaiting decoding, and decoded commits awaiting handling, beyond
/// which they are refused.
pub const COMMIT_QUEUE: usize = 1024;

/// The voting, commit decoding, commit handling and schedule execution
/// processors, each on its own thread, voting and decoding sharing the cache
/// of decoded transactions. Correct in any interleaving: a commit decoded
/// before its blocks' votes decodes their transactions again. The commit
/// handler gets no store: it decides from its input and its own state alone.
pub struct ConsensusProcessors {
    pub votes: Queue<VoteRequest>,
    pub commits: Queue<CommitRequest>,
    pub cache: Arc<ConsensusTxCache>,
    // Dropped last: stop and join the threads.
    _voter: WorkerHandle,
    _decoder: WorkerHandle,
    _handler: WorkerHandle,
    _executor: WorkerHandle,
}

impl ConsensusProcessors {
    /// The processors over `store`, executing in `epoch`; each commit's
    /// outcome goes to `outcomes`.
    pub fn start(
        store: Arc<store::Store>,
        epoch: Arc<crate::epoch::EpochState>,
        outcomes: impl Sink<CommitOutcome> + 'static,
    ) -> ConsensusProcessors {
        let cache = Arc::new(ConsensusTxCache::new());
        let (votes, vote_inbox) = workqueue::queue(VOTE_QUEUE);
        let (commits, commit_inbox) = workqueue::queue(COMMIT_QUEUE);
        let (to_handle, handle_inbox) = workqueue::queue(COMMIT_QUEUE);
        let voting = {
            let (store, cache) = (store.clone(), cache.clone());
            Worker::new("consensus-votes")
                .run(vote_inbox, move || BlockVoter::new(store, cache), |()| {})
                .spawn()
        };
        let decoder = {
            let cache = cache.clone();
            Worker::new("consensus-decode")
                .run(commit_inbox, move || CommitDecoder::new(cache), to_handle)
                .spawn()
        };
        let (to_execute, execute_inbox) = workqueue::queue(COMMIT_QUEUE);
        let handler = Worker::new("consensus-handler")
            .run(handle_inbox, CommitHandler::new, to_execute)
            .spawn();
        let executor = Worker::new("consensus-execution")
            .run(
                execute_inbox,
                move || ScheduleExecutor::new(store, epoch),
                outcomes,
            )
            .spawn();
        ConsensusProcessors {
            votes,
            commits,
            cache,
            _voter: voting,
            _decoder: decoder,
            _handler: handler,
            _executor: executor,
        }
    }
}
