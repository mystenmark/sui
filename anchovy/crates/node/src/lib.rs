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
mod wiring;

use std::sync::Arc;

use aggregator::Committee;
use checkpoint::ProtocolKey;
use consensus::TransactionClient;
use epoch::EpochRecord;
use execution::epoch_start::SystemObjects;
use messages::transaction::DigestPending;
use state_hasher::StateHashes;
use tokio::sync::watch;
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{CommitRecord, EpochStart};
use validator::consensus::decode::CommitRequest;
use validator::consensus::status::ConsensusTxStatusCache;
use validator::consensus::vote::VoteRequest;
use validator::current::EpochHandle;
use validator::epoch::EpochState;
use validator::epoch_store::{BuilderOutput, ConsensusOutput as OutputWriter, EpochStore};
use validator::pipeline::backpressure::{Backpressure, Limits};
use validator::pipeline::items::{CertifiedCheckpoint, EffectsWaitItem};
use validator::pipeline::keys::EffectsKey;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable, GenerationTable};
use validator::processors::Request;
use validator::server::Consensus;
use waiter::Command;
use wiring::Parts;
use workqueue::{Activity, Queue, WorkerHandle};

/// Each queue's capacity.
pub const QUEUE: usize = 4096;

/// How far consensus may run ahead of committed checkpoints. The queues
/// ahead of the executor hold about one item per chunk in flight, so the
/// chunk limit keeps them under `QUEUE`; the transaction limit is the
/// reference's default `backpressure_threshold`.
pub const BACKPRESSURE: Limits = Limits {
    chunks: QUEUE / 4,
    transactions: 100_000,
};

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
    /// The first commit consensus must deliver: the epoch's first, or after
    /// a restart, the window of commits before the last one written.
    pub replay_from: u32,
    /// What the epoch's queues hold, and its workers have in hand.
    activity: Activity,
}

/// Where consensus delivers an epoch's work: blocks to vote on, and
/// commits. Consensus-core's transaction verifier and commit consumer.
#[derive(Clone)]
pub struct ConsensusOutput {
    pub votes: Queue<VoteRequest>,
    pub commits: Queue<CommitRequest>,
    /// The first commit to deliver: consensus-core's replay point.
    pub replay_from: u32,
    /// The last commit the pipeline handled before it started: the commit
    /// record's, or 0 at the epoch's start.
    pub last_processed: u32,
}

/// What the commit handler starts the epoch from.
///
/// # Panics
/// If the store cannot be read or written, lacks `previous`, or has no
/// shared accumulator root.
fn epoch_start(
    store: &store::Store,
    epoch_store: &EpochStore,
    record: &EpochRecord,
    objects: &SystemObjects,
) -> EpochStart {
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
    let accumulator_root_version = epoch_store
        .start_version(store, &root, initial_shared_version)
        .expect("the accumulator root exists")
        .version;
    EpochStart {
        previous_checkpoint: checkpoint.seq,
        accumulator_root_version,
        accumulator_root_initial_shared_version: initial_shared_version,
        authenticator_initial_shared_version: objects.authenticator_state,
        epoch_start_timestamp_ms: record.epoch_start_timestamp_ms,
        epoch_duration_ms: record.epoch_duration_ms,
    }
}

/// Submits this validator's capabilities for the epoch, as the reference's
/// `SuiNode::monitor_reconfiguration` does at each epoch's start: the
/// protocol versions and system packages it supports, which the epoch's last
/// commit chooses the next version from. On its own thread: a full
/// submission lane blocks it, and the pipeline may be started in a runtime.
fn announce_capabilities(
    store: &store::Store,
    epoch: &EpochState,
    protocol_key: &ProtocolKey,
    system_objects: &SystemObjects,
    consensus: &TransactionClient,
) {
    let notification = execution::upgrade::capability_notification(
        store,
        *protocol_key.public(),
        epoch.params.chain,
        &epoch.params.config,
        execution::upgrade::supported_protocol_versions(),
        system_objects.accumulator_root.is_some(),
    );
    let consensus = consensus.clone();
    std::thread::spawn(move || {
        // Failing only as consensus shuts down, when the epoch is over.
        let _ = consensus.submit_blocking(vec![notification], consensus::Priority::Normal);
    });
}

impl Pipeline {
    /// The pipeline over `store`, in `epoch`, signing checkpoints with
    /// `protocol_key`, certifying them with `committee`'s signatures and
    /// handing them to `state_sync`; as `record`, the epoch's start record,
    /// says it starts. `consensus` starts the epoch's consensus, delivering
    /// to the pipeline, and returns its client, as consensus-core's
    /// `ConsensusAuthority::start` does.
    ///
    /// It resumes after the last commit whose consensus output was written
    /// (`docs/quarantine_design.md`), or from the epoch's start, so
    /// consensus must deliver the commits from `replay_from`: replaying
    /// them rebuilds every stage's state. Transactions in committed
    /// checkpoints are found executed, and checkpoints up to the committed
    /// one are rebuilt but not certified again.
    pub fn start(
        store: &Arc<store::Store>,
        epoch: &Arc<EpochState>,
        epoch_store: &EpochStore,
        consensus_output: OutputWriter,
        builder_output: BuilderOutput,
        protocol_key: &Arc<ProtocolKey>,
        committee: &Arc<Committee>,
        consensus: impl FnOnce(ConsensusOutput) -> TransactionClient,
        state_sync: Queue<CertifiedCheckpoint>,
        record: &EpochRecord,
    ) -> Pipeline {
        // Created only by an epoch's last transaction, so as the epoch
        // started, whatever of it a restart finds committed.
        let system_objects =
            execution::epoch_start::system_objects(store).expect("the store reads");
        let start = epoch_start(store, epoch_store, record, &system_objects);
        let committed_seq = store
            .committed_checkpoint()
            .expect("the store reads")
            .expect("the genesis checkpoint is committed");
        let checkpoints = Arc::new(CheckpointStore::new(
            store.clone(),
            committed_seq,
            epoch_store.clone(),
            builder_output.clone(),
        ));
        let next_to_certify = committed_seq + 1;

        // Where the epoch resumes: after the last commit its consensus
        // output was written through, else from its start, before any of its
        // chunks: heights count from 1 each epoch.
        let committed_height = if committed_seq > record.previous_checkpoint {
            BuiltCheckpoint::decode(
                &store
                    .checkpoint(committed_seq)
                    .expect("the store reads")
                    .expect("the committed checkpoint is stored"),
            )
            .last_height
        } else {
            0
        };
        consensus_output
            .committed_checkpoints()
            .committed_through(committed_height);
        builder_output.delete_state_hashes_above(committed_seq);
        let resumed = consensus_output
            .record()
            .map(|bytes| CommitRecord::decode(&bytes));
        // The prior commits rebuild the commit interval observer.
        let window = epoch
            .params
            .config
            .get_consensus_commit_rate_estimation_window_size();
        let replay_from = resumed.map_or(1, |r| (r.commit_index + 1).saturating_sub(window).max(1));
        let state_hashes = Arc::new(StateHashes::new(epoch_store.clone(), builder_output));
        let (first_hashed, root_state) = match resumed {
            Some(r) if r.next_seq - 1 > record.previous_checkpoint => {
                let root = state_hashes.committed_root(
                    store,
                    r.next_seq - 1,
                    record.previous_checkpoint,
                    &state_hasher::decode(&record.root_state),
                );
                (r.next_seq, state_hasher::encode(&root))
            }
            _ => (record.previous_checkpoint + 1, record.root_state.clone()),
        };

        let activity = Activity::new();
        let (queues, inboxes) = wiring::queues(&activity);
        let consensus = consensus(ConsensusOutput {
            votes: queues.votes.clone(),
            commits: queues.commits.clone(),
            replay_from,
            last_processed: resumed.map_or(0, |r| r.commit_index),
        });
        let parts = Arc::new(Parts {
            store: store.clone(),
            epoch: epoch.clone(),
            epoch_store: epoch_store.clone(),
            key: protocol_key.clone(),
            committee: committee.clone(),
            stakes: record.stakes().collect(),
            system_objects,
            consensus: consensus.clone(),
            cache: Arc::new(ConsensusTxCache::new()),
            generations: Arc::new(GenerationTable::new(epoch_store.clone())),
            chunks: Arc::new(ChunkTable::new()),
            checkpoints,
            statuses: Arc::new(ConsensusTxStatusCache::new()),
            state_hashes,
            backpressure: Arc::new(Backpressure::new(BACKPRESSURE, committed_seq)),
            start,
            next_to_certify,
            first_hashed,
            root_state,
        });
        if record
            .stakes()
            .any(|(name, _)| name == *protocol_key.public())
        {
            announce_capabilities(
                store,
                epoch,
                protocol_key,
                &parts.system_objects,
                &consensus,
            );
        }
        let (ended_sender, ended) = watch::channel(None);
        let workers = wiring::spawn(
            parts.clone(),
            &queues,
            inboxes,
            consensus_output,
            state_sync,
            ended_sender,
        );

        Pipeline {
            transactions: queues.transactions,
            votes: queues.votes,
            commits: queues.commits,
            effects: queues.effects_waits,
            statuses: parts.statuses.clone(),
            consensus,
            store: store.clone(),
            epoch: epoch.clone(),
            cache: parts.cache.clone(),
            generations: parts.generations.clone(),
            chunks: parts.chunks.clone(),
            checkpoints: parts.checkpoints.clone(),
            state_hashes: parts.state_hashes.clone(),
            ended,
            replay_from,
            activity,
            _workers: workers,
        }
    }

    /// Blocks until every item ready anywhere in the pipeline is processed,
    /// waiters' ready items included; items waiting on keys stay. Pushes
    /// from outside (consensus, RPC) must have stopped.
    pub fn drain(&self) {
        self.activity.wait_idle();
    }

    /// What the RPC service uses of this pipeline: submissions checked here
    /// and submitted to consensus, waits answered from its effects and the
    /// consensus statuses of its commits.
    pub fn handle(&self) -> Arc<EpochHandle> {
        Arc::new(EpochHandle {
            epoch: self.epoch.clone(),
            transactions: self.transactions.clone(),
            consensus: Consensus {
                client: self.consensus.clone(),
                statuses: self.statuses.clone(),
                effects: self.effects.clone(),
                store: self.store.clone(),
            },
        })
    }
}
