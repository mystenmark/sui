// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Handling a decoded consensus commit (see `decode`), then executing it.
//!
//! `CommitHandler` is the consensus handler: a replicated state machine. It
//! reads only its input (decoded commits, the epoch's parameters) and state
//! it wrote itself while handling earlier commits, and holds no handle to
//! the store, so nothing a validator executed, or when, can change what it
//! decides. It deduplicates by its own record of processed transactions,
//! locks owned inputs in consensus order, dropping a transaction whose
//! owned input an earlier one locked, and schedules the rest (`sequence`:
//! the prologue, the protocol's order, shared-object ordinals). `ScheduleExecutor` then executes the schedule one
//! transaction at a time, as `SubmitTransaction` executes them, skipping
//! transactions executed already. Executing at the versions the ordinals
//! name, the prologue transaction, and congestion control are not here yet.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use ::consensus::{BlockRef, TransactionIndex};
use messages::base::ObjectRef;
use workqueue::{Processor, Refusal, Refuse};

use crate::checks;
use crate::consensus::decode::{CommittedTransaction, DecodedCommit, DecodedRequest, OwnedInputs};
use crate::consensus::sequence::{
    Prologue, Schedule, Scheduled, Sequencer, SharedAccess, SharedOrdinal,
};
use crate::consensus::status::{ConsensusTxStatus, ConsensusTxStatusCache};
use crate::epoch::{EpochParams, EpochState};
use crate::pipeline::items::{
    Chunk, ChunkSettlement, ExecutableTransaction, PendingCheckpoint, ScheduledEntry,
    SettlementInput, SystemKind, SystemTransaction,
};
use crate::pipeline::keys::{AccumulatorVersion, ChunkSettled, TxDigest};
use crate::processors::{Outcome, execute_and_commit, executed};

/// What became of each user transaction a commit carried.
#[derive(Debug)]
pub struct CommitOutcome {
    pub commit_index: u32,
    /// Those scheduled, in schedule order, then those dropped, in consensus
    /// order, then the duplicates.
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
    /// Not executed: see `DropReason`.
    Dropped(DropReason),
    /// Processed in an earlier commit, or executed already.
    Duplicate,
}

/// Why a committed transaction was dropped.
#[derive(Debug)]
pub enum DropReason {
    /// Its owned input `object` was locked by `holder`, earlier in consensus
    /// order: the reference's `ObjectLockConflict`.
    LockConflict { object: ObjectRef, holder: TxDigest },
    /// Its input objects are invalid (the handler), or failed the checks
    /// when its turn came (`ScheduleExecutor`).
    Inputs(validation::Error),
}

/// A commit the handler decided: its schedule, where each scheduled
/// transaction came from, and the transactions it had processed before.
#[derive(Debug)]
pub struct HandledCommit {
    pub commit_index: u32,
    pub schedule: Schedule,
    /// Each scheduled user transaction's block and position, by digest.
    pub origins: HashMap<[u8; 32], (BlockRef, TransactionIndex)>,
    /// Transactions processed in earlier commits, or earlier in this one.
    pub duplicates: Vec<(BlockRef, TransactionIndex, [u8; 32])>,
    /// Transactions dropped before scheduling, in consensus order. Like the
    /// reference's, they are processed: a later inclusion is a duplicate.
    pub dropped: Vec<(BlockRef, TransactionIndex, [u8; 32], DropReason)>,
    pub cache_hits: usize,
    pub cache_misses: usize,
}

/// A handled commit, in the epoch it belongs to.
pub struct HandledRequest {
    pub epoch: Arc<EpochParams>,
    pub commit: HandledCommit,
}

/// A full queue drops the commit; consensus replays commits not handled.
impl Refuse for HandledRequest {
    fn refuse(self, _: Refusal) {}
}

/// The consensus handler. Its state is in memory for now; persisted later,
/// it is written and read back only by the handler.
///
/// Inward: `CommitDecoder`: `DecodedRequest`.
/// Outward: `ScheduleExecutor` (today's serial path): `HandledRequest`. In
/// the pipeline, `CommitStage` wraps it instead.
pub struct CommitHandler {
    /// The index of the last commit handled; 0 before the first.
    last_commit: u32,
    /// The digests of the user transactions processed so far. Only looked
    /// up, never iterated, so its order cannot reach an output.
    processed: HashSet<[u8; 32]>,
    /// The owned references locked this epoch, and the transaction holding
    /// each: the reference's `owned_object_locked_transactions`. Only looked
    /// up, never iterated.
    locks: HashMap<ObjectRef, TxDigest, foldhash::fast::RandomState>,
    sequencer: Sequencer,
}

impl Default for CommitHandler {
    fn default() -> CommitHandler {
        CommitHandler::new()
    }
}

impl CommitHandler {
    pub fn new() -> CommitHandler {
        CommitHandler {
            last_commit: 0,
            processed: HashSet::new(),
            locks: HashMap::default(),
            sequencer: Sequencer::new(),
        }
    }

    /// Decides `commit`, or nothing if it was handled before. It takes only
    /// what consensus agrees on: the epoch's parameters and the commit.
    pub fn handle(&mut self, epoch: &EpochParams, commit: DecodedCommit) -> Option<HandledCommit> {
        // 1. A commit handled before is skipped.
        if commit.commit_index <= self.last_commit {
            return None;
        }
        self.last_commit = commit.commit_index;
        let prologue = Prologue {
            epoch: epoch.epoch,
            round: commit.leader.round,
            commit_index: commit.commit_index,
            timestamp_ms: commit.timestamp_ms,
            commit_digest: commit.digest,
        };

        // 2. Deduplicate against the transactions processed before: this
        //    handler's own record, never the store. Then lock owned inputs in
        //    consensus order, as the reference's `filter_consensus_txns`.
        //
        //    The reference locks before deduplicating, and a lock held by the
        //    same digest is no conflict. Deduplicating first decides the
        //    same: a repeat either won all its locks or was dropped, and is
        //    dropped again either way.
        let mut accepted = Vec::with_capacity(commit.transactions.len());
        let mut origins = HashMap::with_capacity(commit.transactions.len());
        let mut duplicates = Vec::new();
        let mut dropped = Vec::new();
        for CommittedTransaction {
            block,
            index,
            transaction,
            owned,
        } in commit.transactions
        {
            let digest = transaction.get().0.digest().bytes;
            if !self.processed.insert(digest) {
                duplicates.push((block, index, digest));
                continue;
            }
            if let Err(drop) = self.lock(digest, owned) {
                dropped.push((block, index, digest, drop));
                continue;
            }
            origins.insert(digest, (block, index));
            accepted.push(transaction);
        }

        // 3. The schedule: the prologue, the transactions in protocol order,
        //    shared accesses with their ordinals.
        let schedule = self.sequencer.schedule(&epoch.config, prologue, accepted);
        Some(HandledCommit {
            commit_index: commit.commit_index,
            schedule,
            origins,
            duplicates,
            dropped,
            cache_hits: commit.cache_hits,
            cache_misses: commit.cache_misses,
        })
    }

    /// Locks `owned` for `digest`, unless one of them is locked already:
    /// `try_acquire_owned_object_locks_post_consensus`. Every reference is
    /// checked before any is locked, so a transaction listing one twice
    /// does not conflict with itself.
    fn lock(&mut self, digest: TxDigest, owned: OwnedInputs) -> Result<(), DropReason> {
        let owned = owned.map_err(DropReason::Inputs)?;
        // Most transactions lock only their gas coin: one probe.
        if let [object] = owned[..] {
            return match self.locks.entry(object) {
                Entry::Occupied(held) => Err(DropReason::LockConflict {
                    object,
                    holder: *held.get(),
                }),
                Entry::Vacant(free) => {
                    free.insert(digest);
                    Ok(())
                }
            };
        }
        if let Some((object, holder)) = owned
            .iter()
            .find_map(|r| self.locks.get(r).map(|holder| (*r, *holder)))
        {
            return Err(DropReason::LockConflict { object, holder });
        }
        self.locks.extend(owned.into_iter().map(|r| (r, digest)));
        Ok(())
    }
}

impl Processor for CommitHandler {
    type Input = DecodedRequest;
    type Output = HandledRequest;

    fn process(&mut self, request: DecodedRequest) -> Option<HandledRequest> {
        let commit = self.handle(&request.epoch, request.commit)?;
        Some(HandledRequest {
            epoch: request.epoch,
            commit,
        })
    }
}

/// Executes handled commits against the store, serially in schedule order,
/// which gives the versions the ordinals name.
///
/// Today's serial path, which the pipeline's resolver, input waiter and
/// `Executor` replace.
/// Inward: `CommitHandler`: `HandledRequest`.
/// Outward: `CommitOutcome` to its sink.
pub struct ScheduleExecutor {
    store: Arc<store::Store>,
    /// The epoch it executes in, with this node's executor.
    epoch: Arc<EpochState>,
}

impl ScheduleExecutor {
    pub fn new(store: Arc<store::Store>, epoch: Arc<EpochState>) -> ScheduleExecutor {
        ScheduleExecutor { store, epoch }
    }

    /// Executes `commit`'s schedule. A transaction executed already (before
    /// a restart, say) is a duplicate here; the prologue is not executed
    /// yet.
    pub fn execute(&mut self, commit: HandledCommit) -> CommitOutcome {
        let epoch = &*self.epoch;
        let HandledCommit {
            commit_index,
            schedule,
            origins,
            duplicates,
            dropped,
            cache_hits,
            cache_misses,
        } = commit;
        let mut transactions =
            Vec::with_capacity(schedule.len() + dropped.len() + duplicates.len());
        for scheduled in schedule.transactions {
            let Scheduled::User(transaction) = scheduled else {
                continue;
            };
            let digest = transaction.get().0.digest().bytes;
            let (block, index) = origins[&digest];
            let fate = if executed(&self.store, &digest).is_some() {
                Fate::Duplicate
            } else {
                match checks::check_inputs(epoch, &self.store, transaction) {
                    Ok(checked) => Fate::Executed(execute_and_commit(&self.store, epoch, &checked)),
                    Err(e) => Fate::Dropped(DropReason::Inputs(e)),
                }
            };
            transactions.push(Committed {
                block,
                index,
                digest,
                fate,
            });
        }
        transactions.extend(
            dropped
                .into_iter()
                .map(|(block, index, digest, drop)| Committed {
                    block,
                    index,
                    digest,
                    fate: Fate::Dropped(drop),
                }),
        );
        transactions.extend(
            duplicates
                .into_iter()
                .map(|(block, index, digest)| Committed {
                    block,
                    index,
                    digest,
                    fate: Fate::Duplicate,
                }),
        );
        CommitOutcome {
            commit_index,
            transactions,
            cache_hits,
            cache_misses,
        }
    }
}

impl Processor for ScheduleExecutor {
    type Input = HandledRequest;
    type Output = CommitOutcome;

    /// # Panics
    /// On a commit of another epoch: reconfiguration does not exist yet.
    fn process(&mut self, request: HandledRequest) -> Option<CommitOutcome> {
        assert!(
            Arc::ptr_eq(&request.epoch, &self.epoch.params),
            "a commit of the executor's epoch"
        );
        Some(self.execute(request.commit))
    }
}

/// The accumulator root (`SUI_ACCUMULATOR_ROOT_OBJECT_ID`), which each
/// chunk's settlement writes.
const ACCUMULATOR_ROOT: messages::base::ObjectId = messages::base::ObjectId::from_u16(0xacc);

/// What the epoch agreed on at its start that `CommitStage` needs, beyond
/// `EpochParams`. Agreed like it, so reading it keeps the stage pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochStart {
    /// The checkpoint before the epoch's first (the previous epoch's last, or
    /// the genesis checkpoint, 0): the epoch's first checkpoint follows it,
    /// its first chunk follows `previous_height`, and its checkpoint
    /// intervals count from `previous_timestamp_ms`. A restart replays the
    /// epoch's commits from its first, rebuilding the handler's other state.
    pub previous_checkpoint: u64,
    pub previous_height: u64,
    pub previous_timestamp_ms: u64,
    /// The accumulator root's stream starts at this version.
    pub accumulator_root_initial_shared_version: u64,
    /// The root's version the epoch's first chunk reads.
    pub accumulator_root_version: u64,
}

/// What `CommitStage` decided about one commit, in emission order.
#[derive(Debug)]
pub struct Decided {
    /// The commit's chunks, by height.
    pub chunks: Vec<Chunk>,
    /// Each chunk's settlement work, by height.
    pub settlements: Vec<ChunkSettlement>,
    /// The checkpoints the commit completed, by sequence number. A commit
    /// may complete none (its chunks wait for a later commit's), or several.
    pub checkpoints: Vec<PendingCheckpoint>,
    /// The transactions dropped before scheduling, in consensus order. They
    /// are in no chunk, so never in a checkpoint.
    pub dropped: Vec<(BlockRef, TransactionIndex, TxDigest, DropReason)>,
}

/// The consensus handler as the pipeline runs it: decides each commit (see
/// `CommitHandler`), then chunks its schedule, gives the chunks heights and
/// checkpoint sequence numbers (the reference's `CheckpointQueue`), and
/// builds the prologue transaction; and emits the results. Like
/// `CommitHandler`, it reads only its input and state it wrote itself.
///
/// Not here yet, as the design leaves them for later:
/// - deferral and cancellation: the prologue lists no cancelled
///   transactions, and its additional state digest folds in no execution
///   time estimates, so it matches the reference's only for a commit with
///   no user transactions (the reference estimates each one's cost);
/// - randomness: transactions using it are scheduled last in the commit's
///   chunks, not in chunks of their own;
/// - the end of the epoch: no checkpoint is the last of its epoch, and a
///   commit's timestamp is not clamped to the epoch's start.
///
/// Inward:
/// - `CommitDecoder`: `DecodedRequest`.
///
/// Outward:
/// - `VersionResolver`: each chunk's transactions (`Chunk`);
/// - `SettlementScheduler`: each chunk's settlement work
///   (`SettlementInput::Chunk`);
/// - checkpoint waiter, wait for: each `PendingCheckpoint`, keyed by
///   `ChunkSettled` of each of its chunks.
///
/// Writes: the consensus status cache, each committed transaction's status
/// by its position, and the committed leader round. Written, never read: the
/// handler stays a state machine over its input.
pub struct CommitStage {
    handler: CommitHandler,
    start: EpochStart,
    queue: CheckpointQueue,
    commit_times: CommitIntervalObserver,
    /// The last commit decided. Accumulator versions count commits, so
    /// commits must come without gaps.
    last_commit: Option<u32>,
    /// The accumulator root's version the next chunk reads. Each chunk's
    /// barrier, its root's one exclusive writer and with the root its only
    /// input, writes the next version.
    root_version: u64,
    chunks_out: workqueue::Queue<Chunk>,
    settlements_out: workqueue::Queue<SettlementInput>,
    checkpoints_out: workqueue::Queue<waiter::Command<ChunkSettled, PendingCheckpoint>>,
    statuses: Arc<ConsensusTxStatusCache>,
}

impl CommitStage {
    pub fn new(
        start: EpochStart,
        chunks: workqueue::Queue<Chunk>,
        settlements: workqueue::Queue<SettlementInput>,
        checkpoints: workqueue::Queue<waiter::Command<ChunkSettled, PendingCheckpoint>>,
        statuses: Arc<ConsensusTxStatusCache>,
    ) -> CommitStage {
        CommitStage {
            handler: CommitHandler::new(),
            start,
            queue: CheckpointQueue::new(
                start.previous_checkpoint,
                start.previous_height,
                start.previous_timestamp_ms,
            ),
            commit_times: CommitIntervalObserver::default(),
            last_commit: None,
            root_version: start.accumulator_root_version,
            chunks_out: chunks,
            settlements_out: settlements,
            checkpoints_out: checkpoints,
            statuses,
        }
    }

    /// Decides `commit` and lays it out in chunks and checkpoints, or
    /// nothing if it was handled before.
    pub fn decide(&mut self, epoch: &Arc<EpochParams>, commit: DecodedCommit) -> Option<Decided> {
        let HandledCommit {
            mut schedule,
            origins,
            dropped,
            ..
        } = self.handler.handle(epoch, commit)?;
        let config = &epoch.config;
        let Some(&Scheduled::Prologue(prologue)) = schedule.transactions.first() else {
            unreachable!("a schedule starts with its prologue");
        };
        if let Some(last) = self.last_commit {
            assert_eq!(
                prologue.commit_index,
                last + 1,
                "consensus commits come without gaps"
            );
        }
        self.last_commit = Some(prologue.commit_index);

        // 1. The prologue, with the state digest of the commits through
        //    this one.
        self.commit_times.observe(
            config.get_consensus_commit_rate_estimation_window_size(),
            prologue.timestamp_ms,
        );
        let mut prologue_transaction =
            Some(prologue_transaction(&prologue, &self.commit_times.digest()));

        // 2. Each transaction with its shared accesses, in schedule order.
        let transactions = std::mem::take(&mut schedule.transactions);
        let mut entries = Vec::with_capacity(transactions.len());
        let mut digests = Vec::with_capacity(transactions.len());
        for (i, scheduled) in transactions.into_iter().enumerate() {
            let (transaction, digest, origin) = match scheduled {
                Scheduled::Prologue(_) => {
                    let system = prologue_transaction.take().expect("one prologue");
                    let digest = system.digest;
                    (ExecutableTransaction::System(system), digest, None)
                }
                Scheduled::User(transaction) => {
                    let digest = transaction.get().0.digest().bytes;
                    let origin = origins[&digest];
                    (
                        ExecutableTransaction::User(transaction),
                        digest,
                        Some(origin),
                    )
                }
            };
            digests.push(digest);
            entries.push(ScheduledEntry {
                transaction,
                shared: schedule.accesses(i).to_vec(),
                origin,
            });
        }

        // 3. Chunks and checkpoints.
        let mut decided = self.chunk(epoch, &prologue, entries, digests);
        decided.dropped = dropped;
        Some(decided)
    }

    /// Chunks of at most `max_transactions_per_checkpoint`, each with the
    /// next height, queued into checkpoints: the reference's
    /// `create_pending_checkpoints`.
    fn chunk(
        &mut self,
        epoch: &Arc<EpochParams>,
        prologue: &Prologue,
        entries: Vec<ScheduledEntry>,
        digests: Vec<TxDigest>,
    ) -> Decided {
        let config = &epoch.config;
        let max = usize::try_from(config.max_transactions_per_checkpoint())
            .expect("a checkpoint's size fits in usize");
        let chunk_count = entries.len().div_ceil(max);
        let mut decided = Decided {
            chunks: Vec::with_capacity(chunk_count),
            settlements: Vec::with_capacity(chunk_count),
            checkpoints: Vec::new(),
            dropped: Vec::new(),
        };
        let mut flushed = Vec::new();
        let mut entries = entries.into_iter();
        let mut digests = digests.into_iter();
        for position in 0..chunk_count {
            let entries: Vec<_> = entries.by_ref().take(max).collect();
            let digests: Vec<_> = digests.by_ref().take(max).collect();
            let height = self.queue.next_height();
            let checkpoint_seq = self.queue.push_chunk(
                height,
                entries.len(),
                prologue.timestamp_ms,
                max,
                &mut flushed,
            );
            let version = AccumulatorVersion(u64::from(prologue.commit_index));
            let root_version = self.root_version;
            self.root_version += 1;
            decided.settlements.push(ChunkSettlement {
                epoch: epoch.clone(),
                version,
                root_version,
                last_of_commit: position + 1 == chunk_count,
                round: prologue.round,
                height,
                checkpoint_seq,
                digests,
                // One exclusive writer of the root per chunk, its barrier:
                // the commit's `position`th chunk reads generation
                // `position`.
                root: SharedAccess {
                    id: ACCUMULATOR_ROOT,
                    initial_shared_version: self.start.accumulator_root_initial_shared_version,
                    mutability: messages::transaction::SharedObjectMutability::Mutable,
                    ordinal: SharedOrdinal {
                        round: prologue.round,
                        writes_before: u32::try_from(position).expect("fewer than 2^32 chunks"),
                    },
                },
            });
            decided.chunks.push(Chunk {
                epoch: epoch.clone(),
                version,
                root_version,
                round: prologue.round,
                height,
                checkpoint_seq,
                entries,
            });
        }
        let min_interval = config.min_checkpoint_interval_ms.unwrap_or(0);
        flushed.extend(self.queue.flush(prologue.timestamp_ms, min_interval));
        decided.checkpoints = flushed
            .into_iter()
            .map(|f| PendingCheckpoint {
                epoch: epoch.clone(),
                seq: f.seq,
                heights: f.heights,
                timestamp_ms: f.timestamp_ms,
                last_of_epoch: false,
            })
            .collect();
        decided
    }

    fn emit(&mut self, decided: Decided) {
        // Dropped transactions go nowhere: no processor waits on them yet.
        let Decided {
            chunks,
            settlements,
            checkpoints,
            dropped: _,
        } = decided;
        for chunk in chunks {
            push(&self.chunks_out, chunk, "resolver");
        }
        for settlement in settlements {
            push(
                &self.settlements_out,
                SettlementInput::Chunk(settlement),
                "settlement",
            );
        }
        if !checkpoints.is_empty() {
            let keys = checkpoints.iter().map(|c| c.heights.len()).sum();
            let mut batch = waiter::WaitBatch::with_capacity(checkpoints.len(), keys);
            for checkpoint in checkpoints {
                let heights: Vec<_> = checkpoint
                    .heights
                    .iter()
                    .map(|&h| ChunkSettled(h))
                    .collect();
                batch.push(checkpoint, heights);
            }
            push(
                &self.checkpoints_out,
                waiter::Command::Wait(batch),
                "checkpoint waiter",
            );
        }
    }
}

/// Inside the pipeline a full queue is a sizing bug: the handler's state
/// has moved past the commit, so its outputs cannot be dropped. A closed
/// queue means the pipeline is shutting down.
fn push<T>(queue: &workqueue::Queue<T>, item: T, name: &str) {
    if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
        panic!("the {name} queue refused work");
    }
}

impl Processor for CommitStage {
    type Input = DecodedRequest;
    type Output = ();

    fn process(&mut self, request: DecodedRequest) -> Option<()> {
        let DecodedRequest { epoch, mut commit } = request;
        let leader_round = commit.leader.round;
        let rejected = std::mem::take(&mut commit.rejected);
        let positions: Vec<(BlockRef, TransactionIndex)> = commit
            .transactions
            .iter()
            .map(|t| (t.block, t.index))
            .collect();
        let decided = self.decide(&epoch, commit)?;
        // What became of each transaction, for its submitter: dropped, or
        // finalized (scheduled, or a duplicate of one that was).
        let dropped: HashSet<(BlockRef, TransactionIndex)> =
            decided.dropped.iter().map(|(b, i, ..)| (*b, *i)).collect();
        self.statuses.set_statuses(
            rejected
                .into_iter()
                .map(|(b, i)| (b, i, ConsensusTxStatus::Rejected))
                .chain(positions.into_iter().map(|(b, i)| {
                    let status = if dropped.contains(&(b, i)) {
                        ConsensusTxStatus::Dropped
                    } else {
                        ConsensusTxStatus::Finalized
                    };
                    (b, i, status)
                })),
        );
        self.emit(decided);
        self.statuses.update_leader_round(leader_round);
        Some(())
    }
}

/// A checkpoint the queue completed.
#[derive(Debug, PartialEq, Eq)]
struct Flushed {
    seq: u64,
    heights: Vec<u64>,
    timestamp_ms: u64,
}

/// Gathers chunks into checkpoints, as the reference's `CheckpointQueue`:
/// a checkpoint is cut when the next chunk would take it past the
/// transaction limit, and after a commit once `min_checkpoint_interval_ms`
/// has passed since the last one was cut, both by commit timestamps.
struct CheckpointQueue {
    last_built_timestamp: u64,
    /// The heights of the chunks queued, and the timestamp of the last.
    pending: Vec<u64>,
    pending_timestamp: u64,
    pending_transactions: usize,
    height: u64,
    next_seq: u64,
}

impl CheckpointQueue {
    fn new(
        previous_checkpoint: u64,
        previous_height: u64,
        previous_timestamp_ms: u64,
    ) -> CheckpointQueue {
        CheckpointQueue {
            last_built_timestamp: previous_timestamp_ms,
            pending: Vec::new(),
            pending_timestamp: 0,
            pending_transactions: 0,
            height: previous_height,
            next_seq: previous_checkpoint + 1,
        }
    }

    fn next_height(&mut self) -> u64 {
        self.height += 1;
        self.height
    }

    /// Queues the chunk at `height`, first cutting the queued chunks into a
    /// checkpoint if it would not fit with them. Returns the sequence
    /// number of the checkpoint it joins.
    fn push_chunk(
        &mut self,
        height: u64,
        transactions: usize,
        timestamp_ms: u64,
        max: usize,
        flushed: &mut Vec<Flushed>,
    ) -> u64 {
        if self.pending_transactions > 0 && self.pending_transactions + transactions > max {
            flushed.extend(self.flush_forced());
        }
        self.pending_transactions += transactions;
        self.pending.push(height);
        self.pending_timestamp = timestamp_ms;
        self.next_seq
    }

    /// Cuts the queued chunks into a checkpoint if `min_interval_ms` has
    /// passed since the last one, by commit time.
    fn flush(&mut self, timestamp_ms: u64, min_interval_ms: u64) -> Option<Flushed> {
        if timestamp_ms < self.last_built_timestamp + min_interval_ms {
            return None;
        }
        self.flush_forced()
    }

    fn flush_forced(&mut self) -> Option<Flushed> {
        if self.pending.is_empty() {
            return None;
        }
        let flushed = Flushed {
            seq: self.next_seq,
            heights: std::mem::take(&mut self.pending),
            timestamp_ms: self.pending_timestamp,
        };
        self.last_built_timestamp = self.pending_timestamp;
        self.pending_transactions = 0;
        self.next_seq += 1;
        Some(flushed)
    }
}

/// The reference's `AdditionalConsensusState`: the timestamps of the last
/// `window` commits, whose digest each prologue records.
#[derive(Default)]
struct CommitIntervalObserver {
    timestamps: VecDeque<u64>,
}

impl CommitIntervalObserver {
    /// # Panics
    /// If `window` is 0: the reference requires the prologue to record the
    /// state digest, which every protocol version doing so pairs with a
    /// window of 10.
    fn observe(&mut self, window: u32, timestamp_ms: u64) {
        assert!(window > 0, "a commit rate estimation window");
        if self.timestamps.len() >= window as usize {
            self.timestamps.pop_front();
        }
        self.timestamps.push_back(timestamp_ms);
    }

    /// The reference's `AdditionalConsensusState::digest` folded, by its
    /// `IndirectStateObserver::fold_with`, with an observer that saw
    /// nothing.
    ///
    /// TODO: the reference's observer also sees each user transaction's
    /// execution-time estimate (congestion control), so this matches it
    /// only for commits without user transactions.
    fn digest(&self) -> [u8; 32] {
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest as _};
        let state = Blake2b::<U32>::digest(
            bcs::to_bytes(&self.timestamps).expect("a sequence of u64 serializes"),
        );
        let indirect = Blake2b::<U32>::digest([]);
        let mut fold = Blake2b::<U32>::new();
        // Each digest as BCS: its length, then its bytes.
        fold.update([32]);
        fold.update(state);
        fold.update([32]);
        fold.update(indirect);
        fold.finalize().into()
    }
}

/// The consensus commit prologue of a commit, as the reference's
/// `ConsensusCommitInfo::create_consensus_commit_prologue_transaction`
/// builds it: a `ConsensusCommitPrologueV4` system transaction listing no
/// cancelled transactions.
pub fn prologue_transaction(
    prologue: &Prologue,
    additional_state_digest: &[u8; 32],
) -> SystemTransaction {
    use messages::build::base::{
        AdditionalConsensusStateDigest, ConsensusCommitDigest, Digest, ObjectDigest, ObjectId,
        SequenceNumber, SuiAddress,
    };
    use messages::build::signature::{EmptySignInfo, GenericSignature};
    use messages::build::system_transaction::{
        ConsensusCommitPrologueV4, ConsensusDeterminedVersionAssignments,
    };
    use messages::build::transaction::{
        GasData, Intent, IntentMessage, SenderSignedData, SenderSignedTransaction, Transaction,
        TransactionData, TransactionDataV1, TransactionExpiration, TransactionKind,
    };
    /// `GAS_PRICE_FOR_SYSTEM_TX`.
    const SYSTEM_GAS_PRICE: u64 = 1;

    let kind = TransactionKind::ConsensusCommitPrologueV4(ConsensusCommitPrologueV4 {
        epoch: prologue.epoch,
        round: u64::from(prologue.round),
        sub_dag_index: None,
        commit_timestamp_ms: prologue.timestamp_ms,
        consensus_commit_digest: ConsensusCommitDigest(Digest(prologue.commit_digest)),
        consensus_determined_version_assignments:
            ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(Vec::new()),
        additional_state_digest: AdditionalConsensusStateDigest(Digest(*additional_state_digest)),
    });
    let sender = SuiAddress([0; 32]);
    let data = TransactionData::V1(TransactionDataV1 {
        kind,
        sender,
        gas_data: GasData {
            payment: vec![(
                ObjectId(messages::build::base::AccountAddress([0; 32])),
                SequenceNumber(0),
                ObjectDigest(Digest([0; 32])),
            )],
            owner: sender,
            price: SYSTEM_GAS_PRICE,
            budget: 0,
        },
        expiration: TransactionExpiration::None,
    });
    let digest = messages::base::Digest::of(
        "TransactionData",
        &bcs::to_bytes(&data).expect("transaction data serializes"),
    )
    .bytes;
    let transaction = Transaction {
        data: SenderSignedData(SenderSignedTransaction {
            intent_message: IntentMessage {
                // `Intent::sui_transaction()`.
                intent: Intent {
                    scope: 0,
                    version: 0,
                    app_id: 0,
                },
                value: data,
            },
            // An all-zero Ed25519 signature: its flag, signature and key.
            tx_signatures: vec![GenericSignature(vec![0; 1 + 64 + 32])],
        }),
        auth_signature: EmptySignInfo {},
    };
    SystemTransaction {
        kind: SystemKind::Prologue {
            round: prologue.round,
        },
        bytes: bcs::to_bytes(&transaction).expect("a transaction serializes"),
        digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights(flushed: &[Flushed]) -> Vec<(u64, Vec<u64>, u64)> {
        flushed
            .iter()
            .map(|f| (f.seq, f.heights.clone(), f.timestamp_ms))
            .collect()
    }

    #[test]
    fn a_chunk_that_does_not_fit_cuts_the_checkpoint_before_it() {
        let mut queue = CheckpointQueue::new(0, 0, 0);
        let mut flushed = Vec::new();
        for (transactions, timestamp) in [(3, 10), (2, 10), (2, 20)] {
            let height = queue.next_height();
            queue.push_chunk(height, transactions, timestamp, 5, &mut flushed);
        }
        // 3 + 2 fit in 5; the third chunk does not, and starts checkpoint 2.
        assert_eq!(heights(&flushed), vec![(1, vec![1, 2], 10)]);
        assert_eq!(queue.next_seq, 2);
        assert_eq!(
            queue.flush(20, 0),
            Some(Flushed {
                seq: 2,
                heights: vec![3],
                timestamp_ms: 20,
            })
        );
    }

    #[test]
    fn a_chunk_larger_than_the_limit_is_a_checkpoint_of_its_own() {
        let mut queue = CheckpointQueue::new(7, 0, 0);
        let mut flushed = Vec::new();
        let height = queue.next_height();
        assert_eq!(queue.push_chunk(height, 9, 0, 5, &mut flushed), 8);
        assert!(flushed.is_empty());
        let height = queue.next_height();
        assert_eq!(queue.push_chunk(height, 1, 0, 5, &mut flushed), 9);
        assert_eq!(heights(&flushed), vec![(8, vec![1], 0)]);
    }

    #[test]
    fn checkpoints_are_cut_once_the_interval_has_passed_by_commit_time() {
        let mut queue = CheckpointQueue::new(0, 0, 0);
        let mut flushed = Vec::new();
        let mut cut = Vec::new();
        for timestamp in [100, 150, 250, 300, 449, 450] {
            let height = queue.next_height();
            queue.push_chunk(height, 1, timestamp, 100, &mut flushed);
            cut.extend(queue.flush(timestamp, 200));
        }
        assert!(flushed.is_empty());
        assert_eq!(
            heights(&cut),
            vec![(1, vec![1, 2, 3], 250), (2, vec![4, 5, 6], 450)]
        );
        assert_eq!(queue.flush(1000, 0), None);
    }

    #[test]
    fn the_commit_state_keeps_the_last_window_of_timestamps() {
        let mut long = CommitIntervalObserver::default();
        let mut short = CommitIntervalObserver::default();
        for t in [1000, 2000, 3000, 4000] {
            long.observe(3, t);
        }
        for t in [2000, 3000, 4000] {
            short.observe(3, t);
        }
        assert_eq!(long.digest(), short.digest());
        short.observe(3, 5000);
        assert_ne!(long.digest(), short.digest());
    }
}
