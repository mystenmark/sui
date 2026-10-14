// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The whole pipeline, wired (`docs/consensus_pipeline.md` and its diagram):
//! every processor and waiter on its own thread, joined by bounded queues.
//! Consensus, RPC clients and state sync stay outside: the pipeline takes
//! their work through the queues it returns and hands them work through the
//! inboxes it returns.
//!
//! A panic in any worker aborts the process: other processors wait on what
//! each produces, so carrying on would only hang them.

use std::sync::Arc;

use aggregator::{CheckpointAggregator, Committee, SummaryAvailability};
use checkpoint::{
    BuiltAvailability, ChunkAvailability, ContentsBuilder, ProtocolKey, SummaryBuilder,
};
use consensus::TransactionClient;
use exec_pool::{EffectsAvailability, EffectsRouter, Executor};
use resolver::{InputAvailability, VersionResolver};
use settlement::{FundsReserver, SettlementScheduler};
use state_hasher::{StateHash, StateHasher, StateHashes};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{CommitStage, EpochStart};
use validator::consensus::decode::{CommitDecoder, CommitRequest};
use validator::consensus::status::ConsensusTxStatusCache;
use validator::consensus::vote::{BlockVoter, VoteRequest};
use validator::epoch::EpochState;
use validator::pipeline::items::{CertifiedCheckpoint, EffectsWaitItem};
use validator::pipeline::keys::EffectsKey;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable, GenerationTable};
use validator::processors::{Request, SignatureVerifier, TransactionValidator};
use validator::server::{Consensus, Validator};
use validator::submit::SubmitChecker;
use waiter::{Command, WaiterProcessor};
use workqueue::{Inbox, Queue, Worker, WorkerHandle};

use messages::transaction::DigestPending;

/// Each queue's capacity.
pub const QUEUE: usize = 4096;

/// The running pipeline: where the black boxes hand it work, where it hands
/// them work, and the tables its processors share.
pub struct Pipeline {
    // Dropped first: stopping the workers before the queues they push to
    // close.
    _workers: Vec<WorkerHandle>,
    /// From RPC clients: submissions.
    pub transactions: Queue<Request<DigestPending>>,
    /// From consensus: blocks to vote on.
    pub votes: Queue<VoteRequest>,
    /// From consensus: commits.
    pub commits: Queue<CommitRequest>,
    /// Into the effects waiter, for `wait_for_effects`.
    pub effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    /// What became of each committed transaction, by position.
    pub statuses: Arc<ConsensusTxStatusCache>,
    consensus: TransactionClient,
    store: Arc<store::Store>,
    epoch: Arc<EpochState>,
    /// To state sync: certified checkpoints.
    pub certified: Inbox<CertifiedCheckpoint>,
    pub cache: Arc<ConsensusTxCache>,
    pub generations: Arc<GenerationTable>,
    pub chunks: Arc<ChunkTable>,
    pub checkpoints: Arc<CheckpointStore>,
    /// Each checkpoint's state hash and the running root, for the end of
    /// the epoch.
    pub state_hashes: Arc<StateHashes>,
}

/// A sink pushing each batch to `queue`. Inside the pipeline a full queue is
/// a sizing bug, not load to shed.
fn into<T: Send + 'static>(queue: Queue<T>, name: &'static str) -> impl FnMut(T) + Send {
    move |item| {
        // A closed queue means the pipeline is shutting down.
        if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
            panic!("the {name} queue refused work");
        }
    }
}

/// The highest checkpoint this node executed, which the pipeline continues
/// after: `genesis`, written first, on a fresh database.
///
/// # Panics
/// If the store cannot be read or written.
fn last_checkpoint(store: &store::Store, genesis: BuiltCheckpoint) -> BuiltCheckpoint {
    if let Some((_, bytes)) = store.last_checkpoint().expect("the store reads") {
        return BuiltCheckpoint::decode(&bytes);
    }
    assert_eq!(genesis.seq, 0, "the genesis checkpoint is checkpoint 0");
    store
        .put_checkpoint(0, genesis.encode())
        .expect("the store writes");
    genesis
}

/// What the commit handler takes from the epoch's start, the checkpoints
/// numbered after `previous_checkpoint`. The first epoch's only:
/// reconfiguration does not exist yet.
///
/// # Panics
/// If the store has no shared accumulator root.
fn epoch_start(store: &store::Store, last: &BuiltCheckpoint) -> EpochStart {
    let root = store
        .live_object(&messages::base::ObjectId::from_u16(0xacc))
        .expect("the store reads")
        .expect("the accumulator root exists");
    let messages::object::Owner::Shared {
        initial_shared_version,
    } = root.get().owner
    else {
        panic!("the accumulator root is shared");
    };
    EpochStart {
        previous_checkpoint: last.seq,
        previous_height: last.last_height,
        previous_timestamp_ms: last.timestamp_ms,
        accumulator_root_version: store
            .live(&messages::base::ObjectId::from_u16(0xacc))
            .expect("the store reads")
            .expect("the accumulator root exists")
            .version,
        accumulator_root_initial_shared_version: initial_shared_version,
    }
}

impl Pipeline {
    /// The pipeline over `store`, in `epoch`, signing checkpoints with
    /// `protocol_key`, certifying them with `committee`'s signatures. It
    /// builds checkpoints from the one after the highest the store holds as
    /// executed; `genesis` is that checkpoint on a fresh database, and is
    /// ignored otherwise.
    ///
    /// Not covered yet: replaying the commits since that checkpoint, to
    /// rebuild the commit handler's and the resolver's state.
    #[allow(clippy::too_many_lines)]
    pub fn start(
        store: &Arc<store::Store>,
        epoch: &Arc<EpochState>,
        protocol_key: &Arc<ProtocolKey>,
        committee: &Arc<Committee>,
        consensus: &TransactionClient,
        genesis: BuiltCheckpoint,
    ) -> Pipeline {
        let cache = Arc::new(ConsensusTxCache::new());
        let generations = Arc::new(GenerationTable::new());
        let chunks = Arc::new(ChunkTable::new());
        let checkpoints = Arc::new(CheckpointStore::new());
        let last = last_checkpoint(store, genesis);
        let start = epoch_start(store, &last);
        checkpoints.insert_built(last);
        let state_hashes = Arc::new(StateHashes::new());
        let statuses = Arc::new(ConsensusTxStatusCache::new());

        // Black boxes.
        let (transactions, validation_inbox) = workqueue::queue(QUEUE);
        let (votes, vote_inbox) = workqueue::queue(QUEUE);
        let (commits, decode_inbox) = workqueue::queue(QUEUE);
        let (to_state_sync, certified) = workqueue::queue(QUEUE);

        // Between processors.
        let (to_signatures, signature_inbox) = workqueue::queue(QUEUE);
        let (to_submit, submit_inbox) = workqueue::queue(QUEUE);
        let (to_handler, handler_inbox) = workqueue::queue(QUEUE);
        let (to_resolver, resolver_inbox) = workqueue::queue(QUEUE);
        let (to_settlement, settlement_inbox) = workqueue::queue(QUEUE);
        let (to_executor, executor_inbox) = workqueue::queue(QUEUE);
        let (to_funds, funds_inbox) = workqueue::queue(QUEUE);
        let (to_contents, contents_inbox) = workqueue::queue(QUEUE);
        let (to_hasher, hasher_inbox) = workqueue::queue(QUEUE);
        let (to_summary, summary_inbox) = workqueue::queue(QUEUE);
        let (to_aggregator, aggregator_inbox) = workqueue::queue(QUEUE);

        // Waiters' commands.
        let (input_waits, input_inbox) = workqueue::queue(QUEUE);
        let (effects_waits, effects_inbox) = workqueue::queue(QUEUE);
        let (checkpoint_waits, checkpoint_inbox) = workqueue::queue(QUEUE);
        let (sequence_waits, sequence_inbox) = workqueue::queue(QUEUE);
        let (summary_waits, summary_waiter_inbox) = workqueue::queue(QUEUE);

        let mut workers = Vec::new();

        // 1. Submission.
        {
            let store = store.clone();
            workers.push(
                Worker::new("submission")
                    .abort_on_panic()
                    .run(validation_inbox, TransactionValidator::new, to_signatures)
                    .run(signature_inbox, SignatureVerifier::new, to_submit)
                    .run(submit_inbox, move || SubmitChecker::new(store), |()| {})
                    .spawn(),
            );
        }

        // 2. Voting.
        {
            let (store, cache) = (store.clone(), cache.clone());
            let signatures = summary_waits.clone();
            workers.push(
                Worker::new("voting")
                    .abort_on_panic()
                    .run(
                        vote_inbox,
                        move || BlockVoter::new(store, cache).with_signatures(signatures),
                        |()| {},
                    )
                    .spawn(),
            );
        }

        // 3. Commit handling.
        {
            let cache = cache.clone();
            let signatures = summary_waits.clone();
            workers.push(
                Worker::new("commit-decode")
                    .abort_on_panic()
                    .run(
                        decode_inbox,
                        move || CommitDecoder::new(cache).with_signatures(signatures),
                        to_handler,
                    )
                    .spawn(),
            );
            let (chunks_out, settlements, waits, statuses) = (
                to_resolver.clone(),
                to_settlement.clone(),
                checkpoint_waits.clone(),
                statuses.clone(),
            );
            workers.push(
                Worker::new("commit-handler")
                    .abort_on_panic()
                    .run(
                        handler_inbox,
                        move || CommitStage::new(start, chunks_out, settlements, waits, statuses),
                        |()| {},
                    )
                    .spawn(),
            );
        }

        // 4. Execution.
        {
            let (inputs, withdrawals) = (input_waits.clone(), to_funds.clone());
            workers.push(
                Worker::new("resolver")
                    .abort_on_panic()
                    .run(
                        resolver_inbox,
                        move || VersionResolver::new(inputs, withdrawals),
                        |()| {},
                    )
                    .spawn(),
            );
            let availability = InputAvailability {
                store: store.clone(),
                generations: generations.clone(),
            };
            workers.push(
                Worker::new("input-waiter")
                    .abort_on_panic()
                    .run(
                        input_inbox,
                        move || WaiterProcessor::new(availability),
                        into(to_executor, "executor"),
                    )
                    .spawn(),
            );
            let (store_x, epoch_x, generations_x) =
                (store.clone(), epoch.clone(), generations.clone());
            let (inputs, effects, funds) =
                (input_waits.clone(), effects_waits.clone(), to_funds.clone());
            workers.push(
                Worker::new("executor")
                    .abort_on_panic()
                    .run(
                        executor_inbox,
                        move || {
                            Executor::new(store_x, epoch_x, generations_x, inputs, effects, funds)
                        },
                        |()| {},
                    )
                    .spawn(),
            );
            let availability = EffectsAvailability {
                store: store.clone(),
            };
            let router = EffectsRouter {
                settlement: to_settlement.clone(),
            };
            workers.push(
                Worker::new("effects-waiter")
                    .abort_on_panic()
                    .run(
                        effects_inbox,
                        move || WaiterProcessor::new(availability),
                        router,
                    )
                    .spawn(),
            );
        }

        // 5. Settlement and funds.
        {
            let (store_s, chunks_s) = (store.clone(), chunks.clone());
            let (effects, inputs, waits, funds) = (
                effects_waits.clone(),
                input_waits.clone(),
                checkpoint_waits.clone(),
                to_funds.clone(),
            );
            workers.push(
                Worker::new("settlement")
                    .abort_on_panic()
                    .run(
                        settlement_inbox,
                        move || {
                            SettlementScheduler::new(
                                store_s, chunks_s, effects, inputs, waits, funds,
                            )
                        },
                        |()| {},
                    )
                    .spawn(),
            );
            let (store_f, inputs) = (store.clone(), input_waits.clone());
            workers.push(
                Worker::new("funds")
                    .abort_on_panic()
                    .run(
                        funds_inbox,
                        move || FundsReserver::new(store_f, inputs),
                        |()| {},
                    )
                    .spawn(),
            );
        }

        // 6. Checkpointing.
        {
            let availability = ChunkAvailability {
                chunks: chunks.clone(),
            };
            workers.push(
                Worker::new("checkpoint-waiter")
                    .abort_on_panic()
                    .run(
                        checkpoint_inbox,
                        move || WaiterProcessor::new(availability),
                        into(to_contents, "contents"),
                    )
                    .spawn(),
            );
            let (chunks_c, store_c, hasher, sequence) = (
                chunks.clone(),
                store.clone(),
                to_hasher.clone(),
                sequence_waits.clone(),
            );
            workers.push(
                Worker::new("contents")
                    .abort_on_panic()
                    .run(
                        contents_inbox,
                        move || ContentsBuilder::new(chunks_c, store_c, hasher, sequence),
                        |()| {},
                    )
                    .spawn(),
            );
            // Read at the end of the epoch, which is not wired yet. Folds from
            // genesis until reconfiguration supplies the epoch's start.
            let state_hashes = state_hashes.clone();
            workers.push(
                Worker::new("state-hasher")
                    .abort_on_panic()
                    .run(
                        hasher_inbox,
                        move || StateHasher::new(state_hashes, 0, StateHash::default()),
                        |()| {},
                    )
                    .spawn(),
            );
            let availability = BuiltAvailability {
                checkpoints: checkpoints.clone(),
            };
            workers.push(
                Worker::new("sequence-waiter")
                    .abort_on_panic()
                    .run(
                        sequence_inbox,
                        move || WaiterProcessor::new(availability),
                        into(to_summary, "summary"),
                    )
                    .spawn(),
            );
            let (store_s, checkpoints_s, key) =
                (store.clone(), checkpoints.clone(), protocol_key.clone());
            let (sequence, summaries, consensus) = (
                sequence_waits.clone(),
                summary_waits.clone(),
                consensus.clone(),
            );
            workers.push(
                Worker::new("summary")
                    .abort_on_panic()
                    .run(
                        summary_inbox,
                        move || {
                            SummaryBuilder::new(
                                store_s,
                                checkpoints_s,
                                key,
                                sequence,
                                summaries,
                                consensus,
                            )
                        },
                        |()| {},
                    )
                    .spawn(),
            );
            let availability = SummaryAvailability {
                checkpoints: checkpoints.clone(),
            };
            workers.push(
                Worker::new("summary-waiter")
                    .abort_on_panic()
                    .run(
                        summary_waiter_inbox,
                        move || WaiterProcessor::new(availability),
                        into(to_aggregator, "aggregator"),
                    )
                    .spawn(),
            );
            let (checkpoints_a, state_sync) = (checkpoints.clone(), to_state_sync);
            let committee = committee.clone();
            workers.push(
                Worker::new("aggregator")
                    .abort_on_panic()
                    .run(
                        aggregator_inbox,
                        move || CheckpointAggregator::new(committee, checkpoints_a, state_sync),
                        |()| {},
                    )
                    .spawn(),
            );
        }

        Pipeline {
            transactions,
            votes,
            commits,
            effects: effects_waits,
            statuses,
            consensus: consensus.clone(),
            store: store.clone(),
            epoch: epoch.clone(),
            certified,
            cache,
            generations,
            chunks,
            checkpoints,
            state_hashes,
            _workers: workers,
        }
    }

    /// The RPC service over this pipeline: submissions checked here and
    /// submitted to consensus, waits answered from its effects and the
    /// consensus statuses of its commits.
    pub fn validator(&self) -> Validator {
        Validator::new(self.epoch.clone(), self.transactions.clone()).with_consensus(Consensus {
            client: self.consensus.clone(),
            statuses: self.statuses.clone(),
            effects: self.effects.clone(),
            store: self.store.clone(),
        })
    }
}
