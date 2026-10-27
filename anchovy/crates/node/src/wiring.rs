// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The pipeline's wiring: its queues, and the threads draining them.

use std::sync::Arc;

use aggregator::{CheckpointAggregator, Committee};
use checkpoint::{
    BuiltAvailability, BuiltRouter, CheckpointCommitter, ChunkAvailability, ContentsBuilder,
    EpochCloser, ProtocolKey, SummaryBuilder,
};
use consensus::TransactionClient;
use exec_pool::{EffectsAvailability, EffectsRouter, Executor};
use execution::epoch_start::SystemObjects;
use messages::transaction::DigestPending;
use resolver::{InputAvailability, VersionResolver};
use settlement::{FundsReserver, SettlementScheduler};
use state_hasher::{StateHasher, StateHashes};
use tokio::sync::watch;
use validator::checks::{Valid, Verified};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{CommitStage, EpochStart};
use validator::consensus::decode::{CommitDecoder, CommitRequest, DecodedCommit};
use validator::consensus::status::ConsensusTxStatusCache;
use validator::consensus::vote::{BlockVoter, VoteRequest};
use validator::epoch::EpochState;
use validator::epoch_store::{CommittedCheckpoints, ConsensusOutput as OutputWriter, EpochStore};
use validator::pipeline::backpressure::Backpressure;
use validator::pipeline::items::{
    BuiltCommand, BuiltWaits, CertifiedCheckpoint, CheckpointContentsBuilt, CheckpointEffects,
    CheckpointSignature, Chunk, EffectsWaitItem, EpochCloserInput, Executable, FundsCommand,
    InputCommand, PendingCheckpoint, SettlementInput,
};
use validator::pipeline::keys::{ChunkSettled, EffectsKey};
use validator::pipeline::tables::{CheckpointStore, ChunkTable, GenerationTable};
use validator::processors::{Request, SignatureVerifier, TransactionValidator};
use validator::submit::SubmitChecker;
use waiter::{Command, WaiterProcessor};
use workqueue::{Activity, Inbox, Processor, Queue, Sink, Worker, WorkerHandle};

use crate::QUEUE;

/// The epoch's threads, a line each: the inbox its processor drains, the
/// processor (with the queues it pushes to), and where its output goes.
/// `docs/consensus_pipeline.md` has the diagram.
// A table, so that the whole topology fits on one screen.
#[rustfmt::skip]
pub(crate) fn spawn(
    parts: Arc<Parts>,
    q: &Queues,
    i: Inboxes,
    output: OutputWriter,
    state_sync: Queue<CertifiedCheckpoint>,
    ended: watch::Sender<Option<u64>>,
) -> Vec<WorkerHandle> {
    let w = Spawner { parts, queues: q.clone() };
    let committed = output.committed_checkpoints();
    vec![
        // Submission.
        w.thread("submission")
            .run(i.transactions, |p, _| TransactionValidator::new(p.epoch.clone()),           into(&q.signatures, "signatures"))
            .run(i.signatures,   |p, _| SignatureVerifier::new(p.epoch.clone()),              into(&q.submit, "submit"))
            .run(i.submit,       |p, _| SubmitChecker::new(p.epoch.clone(), p.store.clone()), drop)
            .spawn(),
        // Consensus.
        w.one("voting",            i.votes,            |p, q| p.voter(q.built_waits),                                  drop),
        w.one("commit-decode",     i.commits,          |p, q| p.decoder(q.built_waits),                                into(&q.handler, "handler")),
        w.one("commit-handler",    i.handler,          move |p, q| p.commit_stage(output, q.resolver, q.settlement, q.checkpoint_waits), drop),
        // Execution.
        w.one("resolver",          i.resolver,         |p, q| p.resolver(q.input_waits, q.funds),                     drop),
        w.one("input-waiter",      i.input_waits,      |p, _| WaiterProcessor::new(p.input_availability()),           into(&q.executor, "executor")),
        w.one("executor",          i.executor,         |p, q| p.executor(q.input_waits, q.effects_waits, q.funds),    drop),
        w.one("effects-waiter",    i.effects_waits,    |p, _| WaiterProcessor::new(p.effects_availability()),         EffectsRouter { settlement: q.settlement.clone(), epoch_closer: q.closer.clone() }),
        // Settlement and funds.
        w.one("settlement",        i.settlement,       |p, q| p.settlement(q.effects_waits, q.input_waits, q.checkpoint_waits, q.funds), drop),
        w.one("funds",             i.funds,            |p, q| p.funds(q.input_waits),                                 drop),
        // Checkpointing.
        w.one("checkpoint-waiter", i.checkpoint_waits, |p, _| WaiterProcessor::new(p.chunk_availability()),           into(&q.contents, "contents")),
        w.one("contents",          i.contents,         |p, q| p.contents(q.hasher, q.built_waits),                    drop),
        w.one("state-hasher",      i.hasher,           |p, q| p.state_hasher(q.closer),                               drop),
        w.one("built-waiter",      i.built_waits,      |p, _| WaiterProcessor::new(p.built_availability()),           BuiltRouter { epoch_closer: q.closer.clone(), aggregator: q.aggregator.clone() }),
        w.one("epoch-closer",      i.closer,           |p, q| p.epoch_closer(q.resolver, q.effects_waits, q.hasher, q.summary), drop),
        w.one("summary",           i.summary,          |p, q| p.summary(q.built_waits),                               drop),
        w.one("aggregator",        i.aggregator,       move |p, q| p.aggregator(state_sync, q.built_waits),           into(&q.committer, "committer")),
        w.one("committer",         i.committer,        move |p, _| p.committer(committed, ended),                     drop),
    ]
}

/// Declares the epoch's queues by name and item: `Queues`, their sending
/// ends, cloned wherever a processor pushes, and `Inboxes`, their receiving
/// ends, each drained by one processor.
macro_rules! queues {
    ($($name:ident: $item:ty,)*) => {
        #[derive(Clone)]
        pub(crate) struct Queues {
            $(pub $name: Queue<$item>,)*
        }

        pub(crate) struct Inboxes {
            $(pub $name: Inbox<$item>,)*
        }

        /// Every queue of the epoch's, counted by `activity`: draining them
        /// all leaves nothing ready.
        pub(crate) fn queues(activity: &Activity) -> (Queues, Inboxes) {
            $(let $name = activity.queue(QUEUE);)*
            (Queues { $($name: $name.0,)* }, Inboxes { $($name: $name.1,)* })
        }
    };
}

queues! {
    // From the black boxes.
    transactions: Request<DigestPending>,
    votes: VoteRequest,
    commits: CommitRequest,
    // Between processors.
    signatures: Request<Valid>,
    submit: Request<Verified>,
    handler: DecodedCommit,
    resolver: Chunk,
    executor: Vec<Executable>,
    settlement: SettlementInput,
    funds: FundsCommand,
    contents: Vec<PendingCheckpoint>,
    hasher: CheckpointEffects,
    closer: EpochCloserInput,
    summary: Vec<CheckpointContentsBuilt>,
    aggregator: Vec<CheckpointSignature>,
    committer: Vec<CertifiedCheckpoint>,
    // Into waiters.
    input_waits: InputCommand,
    effects_waits: Command<EffectsKey, EffectsWaitItem>,
    checkpoint_waits: Command<ChunkSettled, PendingCheckpoint>,
    built_waits: BuiltCommand,
}

/// What the epoch's processors are built from, besides their queues: the
/// store, the epoch, the tables they share and where the epoch starts.
pub(crate) struct Parts {
    pub store: Arc<store::Store>,
    pub epoch: Arc<EpochState>,
    pub epoch_store: EpochStore,
    pub key: Arc<ProtocolKey>,
    pub committee: Arc<Committee>,
    /// Each member's name and stake, by committee index.
    pub stakes: Vec<([u8; 96], u64)>,
    /// The system objects as the epoch started.
    pub system_objects: SystemObjects,
    pub consensus: TransactionClient,
    pub cache: Arc<ConsensusTxCache>,
    pub generations: Arc<GenerationTable>,
    pub chunks: Arc<ChunkTable>,
    pub checkpoints: Arc<CheckpointStore>,
    pub statuses: Arc<ConsensusTxStatusCache>,
    pub state_hashes: Arc<StateHashes>,
    pub backpressure: Arc<Backpressure>,
    pub start: EpochStart,
    /// The first checkpoint the aggregator certifies.
    pub next_to_certify: u64,
    /// The first checkpoint the state hasher folds, and the root it folds
    /// onto (`StateHash` BCS).
    pub first_hashed: u64,
    pub root_state: Vec<u8>,
}

impl Parts {
    fn voter(&self, built: BuiltWaits) -> BlockVoter {
        BlockVoter::new(self.epoch.clone(), self.store.clone(), self.cache.clone())
            .with_signatures(built)
    }

    fn decoder(&self, built: BuiltWaits) -> CommitDecoder {
        CommitDecoder::new(self.epoch.params.clone(), self.cache.clone()).with_signatures(built)
    }

    fn commit_stage(
        &self,
        output: OutputWriter,
        resolver: Queue<Chunk>,
        settlement: Queue<SettlementInput>,
        checkpoint_waits: Queue<Command<ChunkSettled, PendingCheckpoint>>,
    ) -> CommitStage {
        CommitStage::new(
            self.epoch.params.clone(),
            self.start,
            output,
            resolver,
            settlement,
            checkpoint_waits,
            self.statuses.clone(),
            self.stakes.clone(),
        )
        .with_stored_observations(&execution::epoch_start::stored_execution_time_observations(
            &self.store,
            &self.epoch.params.config,
        ))
        .with_backpressure(self.backpressure.clone())
    }

    fn resolver(&self, inputs: Queue<InputCommand>, funds: Queue<FundsCommand>) -> VersionResolver {
        VersionResolver::new(self.epoch.params.clone(), inputs, funds)
    }

    fn input_availability(&self) -> InputAvailability {
        InputAvailability {
            store: self.store.clone(),
            generations: self.generations.clone(),
        }
    }

    fn executor(
        &self,
        inputs: Queue<InputCommand>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        funds: Queue<FundsCommand>,
    ) -> Executor {
        Executor::new(
            self.store.clone(),
            self.epoch.clone(),
            self.epoch_store.clone(),
            self.generations.clone(),
            inputs,
            effects,
            funds,
        )
    }

    fn effects_availability(&self) -> EffectsAvailability {
        EffectsAvailability {
            store: self.store.clone(),
        }
    }

    fn settlement(
        &self,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        inputs: Queue<InputCommand>,
        checkpoint_waits: Queue<Command<ChunkSettled, PendingCheckpoint>>,
        funds: Queue<FundsCommand>,
    ) -> SettlementScheduler {
        SettlementScheduler::new(
            self.epoch.params.clone(),
            self.store.clone(),
            self.chunks.clone(),
            effects,
            inputs,
            checkpoint_waits,
            funds,
        )
    }

    fn funds(&self, inputs: Queue<InputCommand>) -> FundsReserver {
        FundsReserver::new(self.store.clone(), inputs)
    }

    fn chunk_availability(&self) -> ChunkAvailability {
        ChunkAvailability {
            chunks: self.chunks.clone(),
        }
    }

    fn contents(&self, hasher: Queue<CheckpointEffects>, built: BuiltWaits) -> ContentsBuilder {
        ContentsBuilder::new(
            self.epoch.params.clone(),
            self.chunks.clone(),
            self.store.clone(),
            hasher,
            built,
        )
    }

    fn state_hasher(&self, closer: Queue<EpochCloserInput>) -> StateHasher {
        let prior = state_hasher::decode(&self.root_state);
        StateHasher::new(self.state_hashes.clone(), self.first_hashed, prior)
            .with_epoch_closer(closer)
    }

    fn built_availability(&self) -> BuiltAvailability {
        BuiltAvailability {
            checkpoints: self.checkpoints.clone(),
        }
    }

    fn epoch_closer(
        &self,
        resolver: Queue<Chunk>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        hasher: Queue<CheckpointEffects>,
        summary: Queue<Vec<CheckpointContentsBuilt>>,
    ) -> EpochCloser {
        EpochCloser::new(
            self.epoch.params.clone(),
            self.system_objects,
            self.store.clone(),
            self.checkpoints.clone(),
            resolver,
            effects,
            hasher,
            summary,
        )
    }

    fn summary(&self, built: BuiltWaits) -> SummaryBuilder {
        SummaryBuilder::new(
            self.epoch.params.clone(),
            self.checkpoints.clone(),
            self.key.clone(),
            built,
            self.consensus.clone(),
        )
    }

    fn aggregator(
        &self,
        state_sync: Queue<CertifiedCheckpoint>,
        built: BuiltWaits,
    ) -> CheckpointAggregator {
        CheckpointAggregator::new(
            self.committee.clone(),
            self.checkpoints.clone(),
            self.next_to_certify,
            state_sync,
        )
        .with_fork_checks(built)
        .with_backpressure(self.backpressure.clone())
    }

    fn committer(
        &self,
        committed: CommittedCheckpoints,
        ended: watch::Sender<Option<u64>>,
    ) -> CheckpointCommitter {
        CheckpointCommitter::new(
            self.store.clone(),
            self.checkpoints.clone(),
            self.chunks.clone(),
            self.generations.clone(),
            committed,
            self.backpressure.clone(),
            ended,
        )
    }
}

/// Starts threads whose processors are built, on their threads, from the
/// parts and the queues.
struct Spawner {
    parts: Arc<Parts>,
    queues: Queues,
}

/// A thread to be, and the processors it will run.
struct Thread<'a> {
    spawner: &'a Spawner,
    worker: Worker,
}

impl Spawner {
    /// A thread for several processors. Any panic aborts the process: other
    /// processors wait on what each produces.
    fn thread(&self, name: &str) -> Thread<'_> {
        Thread {
            spawner: self,
            worker: Worker::new(name).abort_on_panic(),
        }
    }

    /// A thread for one processor.
    fn one<P: Processor + 'static>(
        &self,
        name: &str,
        inbox: Inbox<P::Input>,
        make: impl FnOnce(&Parts, Queues) -> P + Send + 'static,
        sink: impl Sink<P::Output> + 'static,
    ) -> WorkerHandle {
        self.thread(name).run(inbox, make, sink).spawn()
    }
}

impl Thread<'_> {
    /// Adds a processor draining `inbox`, built from the parts and the
    /// queues, sending what it emits to `sink`.
    fn run<P: Processor + 'static>(
        self,
        inbox: Inbox<P::Input>,
        make: impl FnOnce(&Parts, Queues) -> P + Send + 'static,
        sink: impl Sink<P::Output> + 'static,
    ) -> Self {
        let (parts, queues) = (self.spawner.parts.clone(), self.spawner.queues.clone());
        Thread {
            spawner: self.spawner,
            worker: self.worker.run(inbox, move || make(&parts, queues), sink),
        }
    }

    fn spawn(self) -> WorkerHandle {
        self.worker.spawn()
    }
}

/// A sink pushing each batch to `queue`. Inside the pipeline a full queue is
/// a sizing bug, not load to shed.
fn into<T: Send + 'static>(queue: &Queue<T>, name: &'static str) -> impl FnMut(T) + Send + use<T> {
    let queue = queue.clone();
    move |item| {
        // A closed queue means the pipeline is shutting down.
        if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
            panic!("the {name} queue refused work");
        }
    }
}
