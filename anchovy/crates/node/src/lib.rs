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
use messages::transaction::DigestPending;
use state_hasher::StateHashes;
use tokio::sync::watch;
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::EpochStart;
use validator::consensus::decode::CommitRequest;
use validator::consensus::status::ConsensusTxStatusCache;
use validator::consensus::vote::VoteRequest;
use validator::current::EpochHandle;
use validator::epoch::EpochState;
use validator::epoch_store::{ConsensusOutput as OutputWriter, EpochStore};
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
    /// What the epoch's queues hold, and its workers have in hand.
    activity: Activity,
}

/// Where consensus delivers an epoch's work: blocks to vote on, and
/// commits. Consensus-core's transaction verifier and commit consumer.
#[derive(Clone)]
pub struct ConsensusOutput {
    pub votes: Queue<VoteRequest>,
    pub commits: Queue<CommitRequest>,
}

/// The epoch's start: the checkpoint its first checkpoint follows,
/// `previous`, and what the commit handler starts from.
///
/// # Panics
/// If the store cannot be read or written, lacks `previous`, or has no
/// shared accumulator root.
fn epoch_start(
    store: &store::Store,
    epoch_store: &EpochStore,
    record: &EpochRecord,
) -> (BuiltCheckpoint, EpochStart) {
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
    /// says it starts. `consensus` starts the epoch's consensus, delivering
    /// to the pipeline, and returns its client, as consensus-core's
    /// `ConsensusAuthority::start` does.
    ///
    /// It starts from the epoch's start, so consensus must deliver the
    /// epoch's commits from its first: replaying them rebuilds every
    /// stage's state. Transactions in committed checkpoints are found
    /// executed, and checkpoints up to the committed one are rebuilt but not
    /// certified again.
    pub fn start(
        store: &Arc<store::Store>,
        epoch: &Arc<EpochState>,
        epoch_store: &EpochStore,
        consensus_output: OutputWriter,
        protocol_key: &Arc<ProtocolKey>,
        committee: &Arc<Committee>,
        consensus: impl FnOnce(ConsensusOutput) -> TransactionClient,
        state_sync: Queue<CertifiedCheckpoint>,
        record: &EpochRecord,
    ) -> Pipeline {
        let (first, start) = epoch_start(store, epoch_store, record);
        let checkpoints = Arc::new(CheckpointStore::new());
        checkpoints.insert_built(first);
        let next_to_certify = store
            .committed_checkpoint()
            .expect("the store reads")
            .expect("the genesis checkpoint is committed")
            + 1;

        let activity = Activity::new();
        let (queues, inboxes) = wiring::queues(&activity);
        let consensus = consensus(ConsensusOutput {
            votes: queues.votes.clone(),
            commits: queues.commits.clone(),
        });
        let parts = Arc::new(Parts {
            store: store.clone(),
            epoch: epoch.clone(),
            epoch_store: epoch_store.clone(),
            key: protocol_key.clone(),
            committee: committee.clone(),
            consensus: consensus.clone(),
            cache: Arc::new(ConsensusTxCache::new()),
            generations: Arc::new(GenerationTable::new()),
            chunks: Arc::new(ChunkTable::new()),
            checkpoints,
            statuses: Arc::new(ConsensusTxStatusCache::new()),
            state_hashes: Arc::new(StateHashes::new()),
            start,
            next_to_certify,
            first_hashed: record.previous_checkpoint + 1,
            root_state: record.root_state.clone(),
        });
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
