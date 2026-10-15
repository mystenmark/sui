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
//!
//! The pipeline lives for one epoch (`epoch`).

pub mod epoch;

use epoch::EpochRecord;
use tokio::sync::watch;

use std::sync::Arc;

use aggregator::{CheckpointAggregator, Committee, SummaryAvailability};
use checkpoint::{
    BuiltAvailability, CheckpointCommitter, ChunkAvailability, ContentsBuilder, EpochCloser,
    ProtocolKey, SummaryBuilder,
};
use consensus::TransactionClient;
use exec_pool::{EffectsAvailability, EffectsRouter, Executor};
use resolver::{InputAvailability, VersionResolver};
use settlement::{FundsReserver, SettlementScheduler};
use state_hasher::{StateHasher, StateHashes};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{CommitStage, EpochStart};
use validator::consensus::decode::{CommitDecoder, CommitRequest};
use validator::consensus::status::ConsensusTxStatusCache;
use validator::consensus::vote::{BlockVoter, VoteRequest};
use validator::current::EpochHandle;
use validator::epoch::EpochState;
use validator::pipeline::items::{CertifiedCheckpoint, EffectsWaitItem, EpochCloserInput};
use validator::pipeline::keys::EffectsKey;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable, GenerationTable};
use validator::processors::{Request, SignatureVerifier, TransactionValidator};
use validator::server::Consensus;
use validator::submit::SubmitChecker;
use waiter::{Command, WaiterProcessor};
use workqueue::{Queue, Worker, WorkerHandle};

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
    pub cache: Arc<ConsensusTxCache>,
    pub generations: Arc<GenerationTable>,
    pub chunks: Arc<ChunkTable>,
    pub checkpoints: Arc<CheckpointStore>,
    /// Each checkpoint's state hash and the running root.
    pub state_hashes: Arc<StateHashes>,
    /// The epoch's last checkpoint, once committed: the epoch is over.
    pub ended: watch::Receiver<Option<u64>>,
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

/// The epoch's start: the checkpoint its first checkpoint follows,
/// `previous`, and what the commit handler starts from.
///
/// # Panics
/// If the store cannot be read or written, lacks `previous`, or has no
/// shared accumulator root.
fn epoch_start(store: &store::Store, record: &EpochRecord) -> (BuiltCheckpoint, EpochStart) {
    let epoch = record.epoch;
    let checkpoint = BuiltCheckpoint::decode(
        &store
            .checkpoint(record.previous_checkpoint)
            .expect("the store reads")
            .expect("the epoch's previous checkpoint is committed"),
    );
    let root = messages::base::ObjectId::from_u16(0xacc);
    // Its initial shared version never changes, whatever the store holds.
    let messages::object::Owner::Shared {
        initial_shared_version,
    } = store
        .live_object(&root)
        .expect("the store reads")
        .expect("the accumulator root exists")
        .get()
        .owner
    else {
        panic!("the accumulator root is shared");
    };
    // Version assignment starts here, as for any shared object: the
    // commit handler numbers each chunk's root version from it.
    let accumulator_root_version = store
        .start_version(epoch, &root, initial_shared_version)
        .expect("the store reads and writes")
        .expect("the accumulator root exists")
        .version;
    let start = EpochStart {
        previous_checkpoint: checkpoint.seq,
        previous_height: checkpoint.last_height,
        previous_timestamp_ms: checkpoint.timestamp_ms,
        accumulator_root_version,
        accumulator_root_initial_shared_version: initial_shared_version,
        epoch_start_timestamp_ms: record.epoch_start_timestamp_ms,
        epoch_duration_ms: record.epoch_duration_ms,
    };
    (checkpoint, start)
}

impl Pipeline {
    /// The pipeline over `store`, in `epoch`, signing checkpoints with
    /// `protocol_key`, certifying them with `committee`'s signatures and
    /// handing them to `state_sync`; as `record`, the epoch's start record,
    /// says it starts.
    ///
    /// It starts from the epoch's start, so consensus must deliver the
    /// epoch's commits from its first: replaying them rebuilds every
    /// stage's state. Transactions in committed checkpoints are found
    /// executed, and checkpoints up to the committed one are rebuilt but not
    /// certified again.
    #[allow(clippy::too_many_lines)]
    pub fn start(
        store: &Arc<store::Store>,
        epoch: &Arc<EpochState>,
        protocol_key: &Arc<ProtocolKey>,
        committee: &Arc<Committee>,
        consensus: &TransactionClient,
        state_sync: Queue<CertifiedCheckpoint>,
        record: &EpochRecord,
    ) -> Pipeline {
        let cache = Arc::new(ConsensusTxCache::new());
        let generations = Arc::new(GenerationTable::new());
        let chunks = Arc::new(ChunkTable::new());
        let checkpoints = Arc::new(CheckpointStore::new());
        let (first, start) = epoch_start(store, record);
        checkpoints.insert_built(first);
        let next_to_certify = store
            .committed_checkpoint()
            .expect("the store reads")
            .expect("the genesis checkpoint is committed")
            + 1;
        let state_hashes = Arc::new(StateHashes::new());
        let statuses = Arc::new(ConsensusTxStatusCache::new());

        // Black boxes.
        let (transactions, validation_inbox) = workqueue::queue(QUEUE);
        let (votes, vote_inbox) = workqueue::queue(QUEUE);
        let (commits, decode_inbox) = workqueue::queue(QUEUE);
        let to_state_sync = state_sync;

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
        let (to_closer, closer_inbox) = workqueue::queue(QUEUE);
        let (ended_sender, ended) = watch::channel(None);
        let (to_committer, committer_inbox) = workqueue::queue(QUEUE);
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
                epoch_closer: to_closer.clone(),
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
            let (state_hashes, closer) = (state_hashes.clone(), to_closer.clone());
            let (first, prior) = (
                record.previous_checkpoint + 1,
                state_hasher::decode(&record.root_state),
            );
            workers.push(
                Worker::new("state-hasher")
                    .abort_on_panic()
                    .run(
                        hasher_inbox,
                        move || {
                            StateHasher::new(state_hashes, first, prior).with_epoch_closer(closer)
                        },
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
                        {
                            let closer = into(to_closer.clone(), "epoch closer");
                            let mut closer = closer;
                            move |ready| closer(EpochCloserInput::Built(ready))
                        },
                    )
                    .spawn(),
            );
            let (store_e, checkpoints_e, resolver, effects, hasher) = (
                store.clone(),
                checkpoints.clone(),
                to_resolver.clone(),
                effects_waits.clone(),
                to_hasher.clone(),
            );
            workers.push(
                Worker::new("epoch-closer")
                    .abort_on_panic()
                    .run(
                        closer_inbox,
                        move || {
                            EpochCloser::new(
                                store_e,
                                checkpoints_e,
                                resolver,
                                effects,
                                hasher,
                                to_summary,
                            )
                        },
                        |()| {},
                    )
                    .spawn(),
            );
            let (checkpoints_s, key) = (checkpoints.clone(), protocol_key.clone());
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
                            SummaryBuilder::new(checkpoints_s, key, sequence, summaries, consensus)
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
                        move || {
                            CheckpointAggregator::new(
                                committee,
                                checkpoints_a,
                                next_to_certify,
                                state_sync,
                            )
                        },
                        into(to_committer, "committer"),
                    )
                    .spawn(),
            );
            let (store_c, checkpoints_c, chunks_c) =
                (store.clone(), checkpoints.clone(), chunks.clone());
            workers.push(
                Worker::new("committer")
                    .abort_on_panic()
                    .run(
                        committer_inbox,
                        move || {
                            CheckpointCommitter::new(store_c, checkpoints_c, chunks_c, ended_sender)
                        },
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
            cache,
            generations,
            chunks,
            checkpoints,
            state_hashes,
            ended,
            _workers: workers,
        }
    }

    /// What the RPC service uses of this pipeline: submissions checked here
    /// and submitted to consensus, waits answered from its effects and the
    /// consensus statuses of its commits.
    pub fn handle(&self) -> Arc<EpochHandle> {
        Arc::new(EpochHandle {
            epoch: self.epoch.clone(),
            transactions: self.transactions.clone(),
            consensus: Some(Consensus {
                client: self.consensus.clone(),
                statuses: self.statuses.clone(),
                effects: self.effects.clone(),
                store: self.store.clone(),
            }),
        })
    }
}
