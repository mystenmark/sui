// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Handling a decoded consensus commit (see `decode`).
//!
//! `CommitHandler` is the consensus handler: a replicated state machine. It
//! reads only its input (decoded commits, the epoch's parameters) and state
//! it wrote itself while handling earlier commits, and holds no handle to
//! the store, so nothing a validator executed, or when, can change what it
//! decides. It deduplicates by its own record of processed transactions,
//! locks owned inputs in consensus order, dropping a transaction whose
//! owned input an earlier one locked, and schedules the rest (`sequence`:
//! the prologue, the protocol's order, shared-object ordinals).
//! `CommitStage` lays the schedule out in chunks and checkpoints for the
//! pipeline. Congestion control is not here yet.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use ::consensus::{BlockRef, TransactionIndex};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest as _};
use containers::Bump;
use messages::base::{Hashed, ObjectRef};
use messages::build::system_transaction::{Jwk, JwkId};
use protocol_config::ProtocolConfig;
use workqueue::Processor;

use crate::consensus::decode::{CommittedTransaction, DecodedCommit};
use crate::consensus::deferral::{DeferralKey, Deferred, decode_deferred, encode_deferred};
use crate::consensus::execution_time::{
    ExecutionTimeEstimator, Key, decode_estimates, encode_estimates,
};
use crate::consensus::jwk::{JwkAggregator, JwkVote, decode_vote, encode_vote, jwk_order};
use crate::consensus::sequence::{
    AuthenticatorUpdate, Prologue, Schedule, Scheduled, Sequencer, SharedAccess, SharedOrdinal,
};
use crate::consensus::status::{ConsensusTxStatus, ConsensusTxStatusCache};
use crate::epoch::EpochParams;
use crate::epoch_store::ConsensusOutput;
use crate::pipeline::backpressure::Backpressure;
use crate::pipeline::items::{
    Chunk, ChunkSettlement, EpochClose, ExecutableTransaction, PendingCheckpoint, ScheduledEntry,
    SettlementInput, SystemKind, SystemTransaction,
};
use crate::pipeline::keys::{AccumulatorVersion, ChunkSettled, StreamId, TxDigest};

/// Why a committed transaction was dropped.
#[derive(Debug)]
pub enum DropReason {
    /// Its owned input `object` was locked by `holder`, earlier in consensus
    /// order: the reference's `ObjectLockConflict`.
    LockConflict { object: ObjectRef, holder: TxDigest },
    /// Its input objects are invalid.
    Inputs(validation::Error),
}

/// A commit the handler decided: its schedule, where each scheduled
/// transaction came from, and the transactions it had processed before.
#[derive(Debug)]
pub struct HandledCommit<'b> {
    pub commit_index: u32,
    pub schedule: Schedule<'b>,
    /// Each scheduled user transaction's block and position, by digest.
    pub origins: containers::MessageMap<'b, Hashed, (BlockRef, TransactionIndex)>,
    /// Transactions processed in earlier commits, or earlier in this one.
    pub duplicates: containers::Vec<'b, (BlockRef, TransactionIndex, TxDigest)>,
    /// Transactions deferred to a later round, in schedule order.
    pub deferred: containers::Vec<'b, (BlockRef, TransactionIndex, TxDigest)>,
    /// Transactions dropped before scheduling, in consensus order. Like the
    /// reference's, they are processed: a later inclusion is a duplicate.
    pub dropped: containers::Vec<'b, (BlockRef, TransactionIndex, TxDigest, DropReason)>,
    /// Each scheduled user transaction's execution time estimate, in
    /// microseconds, in schedule order: what the reference's
    /// `IndirectStateObserver` sees.
    pub estimates_us: containers::Vec<'b, u64>,
    pub cache_hits: usize,
    pub cache_misses: usize,
}

/// The consensus handler. What it decided, beyond the commit in hand, is
/// the epoch store's consensus output, which it alone writes and reads:
/// - the user transactions processed this epoch;
/// - the owned references locked this epoch, and the transaction holding
///   each: the reference's `owned_object_locked_transactions`;
/// - each shared-object stream's last scheduled generation;
/// - the transactions deferred to later rounds, by `DeferralKey`;
/// - the execution time observations taken.
///
/// Each is only looked up, or iterated in key order, so no order of a hash
/// table's can reach an output.
///
/// Run inside `CommitStage`, which feeds it decoded commits and lays out
/// what it decides.
pub struct CommitHandler {
    /// The index of the last commit handled; 0 before the first.
    last_commit: u32,
    output: ConsensusOutput,
    sequencer: Sequencer,
    execution_times: ExecutionTimeEstimator,
    jwks: JwkAggregator,
    /// The JSON web keys that became active in the last commit handled,
    /// which the next adds to the authenticator state.
    new_jwks: Vec<(JwkId, Jwk)>,
    /// The authenticator state's initial shared version, if the epoch
    /// agrees keys: the reference's `authenticator_state_enabled`.
    authenticator: Option<u64>,
}

impl CommitHandler {
    /// A handler of `committee`'s commits (each member's name and stake, by
    /// index), that has handled those through `last_commit`, with what the
    /// commits written observed and voted for restored. `authenticator` is
    /// the authenticator state's initial shared version, if it exists.
    pub fn new(
        output: ConsensusOutput,
        last_commit: u32,
        committee: Vec<([u8; 96], u64)>,
        config: &ProtocolConfig,
        authenticator: Option<u64>,
    ) -> CommitHandler {
        let mut execution_times = ExecutionTimeEstimator::new(committee.clone(), config);
        for (generation, author, estimates) in output.execution_time_observations() {
            execution_times.restore(author, generation, &decode_estimates(&estimates));
        }
        let mut jwks =
            JwkAggregator::new(committee, config.max_jwk_votes_per_validator_per_epoch());
        for vote in output.jwk_votes() {
            let (author, key) = decode_vote(&vote);
            jwks.restore(author, key);
        }
        let new_jwks = output
            .new_jwks()
            .map(|keys| bcs::from_bytes(&keys).expect("keys as encoded"))
            .unwrap_or_default();
        CommitHandler {
            last_commit,
            output,
            sequencer: Sequencer::new(),
            execution_times,
            jwks,
            new_jwks,
            authenticator: authenticator.filter(|_| config.enable_jwk_consensus_updates()),
        }
    }

    /// Ends the commit just handled, with what `CommitStage` decided of it:
    /// see `ConsensusOutput::finish_commit`.
    pub fn finish(&mut self, record: &CommitRecord, height: u64, drained: bool) {
        self.output.finish_commit(record.encode(), height, drained);
    }

    /// Decides `commit`, or nothing if it was handled before. It takes only
    /// what consensus agrees on: the epoch's parameters and the commit.
    /// `bump` holds the decision's temporaries.
    pub fn handle<'b>(
        &mut self,
        epoch: &EpochParams,
        commit: DecodedCommit,
        bump: &'b Bump,
    ) -> Option<HandledCommit<'b>> {
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

        let round = u64::from(commit.leader.round);
        let config = &epoch.config;

        // 2. Lock owned inputs in consensus order, as the reference's
        //    `filter_consensus_txns`: a transaction an input of which another
        //    holds is dropped, and if that one took it earlier in this
        //    commit, the holder is contested. Dropped transactions are
        //    processed: a later inclusion is a duplicate.
        let mut taken = containers::hash_map(bump, commit.transactions.len());
        let mut contested = containers::message_set(bump, 0);
        let mut locked = containers::Vec::with_capacity_in(commit.transactions.len(), bump);
        let mut dropped = containers::Vec::new_in(bump);
        for CommittedTransaction {
            block,
            index,
            transaction,
            owned,
        } in commit.transactions
        {
            let digest = *transaction.get().0.digest();
            let owned = match owned {
                Ok(owned) => owned,
                Err(e) => {
                    self.output.set_processed(digest);
                    dropped.push((block, index, digest, DropReason::Inputs(e)));
                    continue;
                }
            };
            if let Err(conflict) = self.lock(digest, &owned) {
                contested.extend(owned.iter().filter_map(|r| taken.get(r).copied()));
                self.output.set_processed(digest);
                dropped.push((block, index, digest, conflict));
                continue;
            }
            taken.extend(owned.iter().map(|r| (*r, digest)));
            locked.push((block, index, transaction));
        }

        // 3. Deduplicate against the transactions processed before, this
        //    handler's own record: `deduplicate_consensus_txns`.
        let mut accepted = containers::Vec::with_capacity_in(locked.len(), bump);
        let mut origins = containers::message_map(bump, locked.len());
        let mut duplicates = containers::Vec::new_in(bump);
        for (block, index, transaction) in locked {
            let digest = *transaction.get().0.digest();
            if self.output.is_processed(&digest) {
                duplicates.push((block, index, digest));
                continue;
            }
            self.output.set_processed(digest);
            origins.insert(digest, (block, index));
            accepted.push(transaction);
        }

        // 4. The commit's execution time observations, before its
        //    transactions are estimated.
        for shared in &commit.observations {
            if self.execution_times.observe(shared) {
                self.output.put_execution_time_observations(
                    shared.author,
                    shared.generation,
                    encode_estimates(&shared.estimates),
                );
            }
        }

        // 5. The keys voted for (`process_jwks`), and the update adding those
        //    that became active in the last commit
        //    (`create_authenticator_state_update`).
        let update = self.jwk_update(&commit.jwks);

        // 6. The transactions deferred to this round, then the commit's, in
        //    protocol order: `merge_and_reorder_transactions`.
        let mut previously_deferred = containers::message_map(bump, 0);
        let mut transactions = containers::Vec::with_capacity_in(accepted.len(), bump);
        for (key, deferred) in self.output.take_deferred(DeferralKey::due_by(round)) {
            for deferred in decode_deferred(&deferred) {
                let hashed = *deferred.transaction.get().0.digest();
                previously_deferred.insert(hashed, key);
                origins.insert(hashed, deferred.origin);
                transactions.push(deferred.transaction);
            }
        }
        transactions.extend(accepted);
        let ordered = self.sequencer.order(config, transactions, bump);

        // 7. Each, in order, deferred or estimated and scheduled:
        //    `handle_deferral_and_cancellation`. A contested holder is
        //    deferred a round, as a penalty for the double spend.
        let mut deferred: containers::BTreeMap<DeferralKey, containers::Vec<Deferred>> =
            containers::BTreeMap::new_in(bump);
        let mut deferred_here = containers::Vec::new_in(bump);
        let mut admitted = containers::Vec::with_capacity_in(ordered.len(), bump);
        let mut estimates_us = containers::Vec::with_capacity_in(ordered.len(), bump);
        for transaction in ordered {
            let hashed = *transaction.get().0.digest();
            if contested.contains(&hashed) && config.defer_owned_object_double_spend() {
                let from = previously_deferred
                    .get(&hashed)
                    .map_or(round, DeferralKey::deferred_from_round);
                let key = DeferralKey::next_round(round, from);
                if key.within_limit(config.max_deferral_rounds_for_congestion_control()) {
                    let origin = origins.remove(&hashed).expect("each transaction's origin");
                    deferred_here.push((origin.0, origin.1, hashed));
                    deferred
                        .entry(key)
                        .or_insert_with(|| containers::Vec::new_in(bump))
                        .push(Deferred {
                            origin,
                            transaction,
                        });
                    continue;
                }
            }
            estimates_us.push(self.execution_times.estimate_us(&transaction));
            admitted.push(transaction);
        }
        for (key, transactions) in deferred {
            self.output.defer(key, encode_deferred(&transactions));
        }

        // 8. The schedule: the prologue, the update, then the transactions,
        //    with shared accesses and their ordinals, linked to earlier
        //    rounds.
        let mut schedule = self.sequencer.schedule(prologue, update, admitted, bump);
        self.link(schedule.all_accesses_mut());
        Some(HandledCommit {
            commit_index: commit.commit_index,
            schedule,
            origins,
            duplicates,
            deferred: deferred_here,
            dropped,
            estimates_us,
            cache_hits: commit.cache_hits,
            cache_misses: commit.cache_misses,
        })
    }

    /// The execution time observations so far, as the epoch's last commit
    /// stores them for the next epoch.
    pub fn execution_time_observations(&self) -> Vec<(Key, Vec<([u8; 96], Duration)>)> {
        self.execution_times.observations()
    }

    /// Counts `votes`, and takes the keys that became active in the last
    /// commit: the update adding them, if any; none unless the epoch agrees
    /// keys.
    fn jwk_update(&mut self, votes: &[JwkVote]) -> Option<AuthenticatorUpdate> {
        let initial_shared_version = self.authenticator?;
        let mut active = Vec::new();
        for vote in votes {
            let Some(activated) = self.jwks.vote(vote) else {
                continue;
            };
            let key = (vote.id.clone(), vote.jwk.clone());
            self.output.put_jwk_vote(encode_vote(vote.author, &key));
            if activated {
                active.push(key);
            }
        }
        self.output
            .set_new_jwks(bcs::to_bytes(&active).expect("keys serialize"));
        let keys = std::mem::replace(&mut self.new_jwks, active);
        (!keys.is_empty()).then_some(AuthenticatorUpdate {
            initial_shared_version,
            keys,
        })
    }

    /// Locks `owned` for `digest`, unless another transaction holds one of
    /// them: `try_acquire_owned_object_locks_post_consensus`. Every
    /// reference is checked before any is locked, so a transaction listing
    /// one twice does not conflict with itself.
    fn lock(&mut self, digest: TxDigest, owned: &[ObjectRef]) -> Result<(), DropReason> {
        if let Some((object, holder)) = owned.iter().find_map(|r| {
            let holder = self.output.lock_holder(r)?;
            (holder != digest).then_some((*r, holder))
        }) {
            return Err(DropReason::LockConflict { object, holder });
        }
        for object in owned {
            self.output.set_lock(*object, digest);
        }
        Ok(())
    }

    /// Sets each generation-0 access's base, the stream's last generation
    /// from an earlier round, and records the generations exclusive writes
    /// produce. Accesses are in schedule order.
    ///
    /// # Panics
    /// On a generation-0 access after an exclusive write of its stream in
    /// the same round: the sequencer counts every earlier write.
    pub fn link(&mut self, accesses: &mut [SharedAccess]) {
        for access in accesses {
            let stream = StreamId {
                id: access.id,
                initial_shared_version: access.initial_shared_version,
            };
            let round = access.ordinal.round;
            let generation = access.ordinal.writes_before;
            if generation == 0 {
                match self.output.stream(&stream) {
                    Some((base_round, base)) if base_round < round => {
                        access.base = Some(SharedOrdinal {
                            round: base_round,
                            writes_before: base,
                        });
                    }
                    // Recorded by an earlier live access of this round.
                    Some((base_round, 0)) if base_round == round => {}
                    Some(_) => panic!(
                        "generation 0 of {stream:?} in round {round} after a write in that round"
                    ),
                    // Later rounds wait for generation 0, which the executor
                    // records when this round first touches the stream: none
                    // of them executes before this round's live readers.
                    None => self.output.set_stream(stream, (round, 0)),
                }
            }
            // A transaction writes a stream once, so its later accesses
            // never see this.
            if access.mutability == messages::transaction::SharedObjectMutability::Mutable {
                self.output.set_stream(stream, (round, generation + 1));
            }
        }
    }
}

/// The accumulator root (`SUI_ACCUMULATOR_ROOT_OBJECT_ID`), which each
/// chunk's settlement writes.
const ACCUMULATOR_ROOT: messages::base::ObjectId = messages::base::ObjectId::from_u16(0xacc);

/// The system state object, which the change-epoch transaction writes, and
/// the version its sharing started at.
const SYSTEM_STATE: messages::base::ObjectId = messages::base::ObjectId::from_u16(0x5);
const SYSTEM_STATE_INITIAL_SHARED_VERSION: u64 = 1;

/// `CommitStage`'s state after a commit, which a restart resumes from: the
/// reference's `last_consensus_stats_v2`. Written with the commit's output,
/// once the checkpoints covering it have committed, and only after a commit
/// that leaves no chunk waiting for a checkpoint: so it is all of the
/// stage's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitRecord {
    pub commit_index: u32,
    pub leader_round: u32,
    /// The last chunk height assigned.
    pub height: u64,
    pub next_seq: u64,
    /// The timestamp of the last checkpoint cut, for the interval cuts.
    pub last_built_timestamp: u64,
    pub root_version: u64,
    pub closed: bool,
}

impl CommitRecord {
    pub fn encode(&self) -> Vec<u8> {
        bcs::to_bytes(&(
            self.commit_index,
            self.leader_round,
            self.height,
            self.next_seq,
            self.last_built_timestamp,
            self.root_version,
            self.closed,
        ))
        .expect("a record encodes")
    }

    /// # Panics
    /// If `bytes` is not what `encode` wrote.
    pub fn decode(bytes: &[u8]) -> CommitRecord {
        let (
            commit_index,
            leader_round,
            height,
            next_seq,
            last_built_timestamp,
            root_version,
            closed,
        ) = bcs::from_bytes(bytes).expect("a stored record decodes");
        CommitRecord {
            commit_index,
            leader_round,
            height,
            next_seq,
            last_built_timestamp,
            root_version,
            closed,
        }
    }
}

/// What the epoch agreed on at its start that `CommitStage` needs, beyond
/// `EpochParams`. Agreed like it, so reading it keeps the stage pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochStart {
    /// The checkpoint before the epoch's first (the previous epoch's last, or
    /// the genesis checkpoint, 0): the epoch's first checkpoint follows it.
    /// Chunk heights count from 1 each epoch, as the reference's, whose
    /// settlement transactions carry them.
    pub previous_checkpoint: u64,
    /// The accumulator root's stream starts at this version.
    pub accumulator_root_initial_shared_version: u64,
    /// The authenticator state's, if it exists: absent before the end of
    /// the epoch that creates it.
    pub authenticator_initial_shared_version: Option<u64>,
    /// The root's version the epoch's first chunk reads: where version
    /// assignment starts it (`Store::start_version`).
    pub accumulator_root_version: u64,
    /// The epoch ends with the first commit at or past
    /// `epoch_start_timestamp_ms + epoch_duration_ms`.
    pub epoch_start_timestamp_ms: u64,
    pub epoch_duration_ms: u64,
}

/// What `CommitStage` decided about one commit, in emission order.
#[derive(Debug)]
pub struct Decided<'b> {
    /// The commit's chunks, by height.
    pub chunks: containers::Vec<'b, Chunk>,
    /// Each chunk's settlement work, by height.
    pub settlements: containers::Vec<'b, ChunkSettlement>,
    /// The checkpoints the commit completed, by sequence number. A commit
    /// may complete none (its chunks wait for a later commit's), or several.
    pub checkpoints: containers::Vec<'b, PendingCheckpoint>,
    /// The transactions dropped before scheduling, in consensus order. They
    /// are in no chunk, so never in a checkpoint.
    pub dropped: containers::Vec<'b, (BlockRef, TransactionIndex, TxDigest, DropReason)>,
}

/// The consensus handler as the pipeline runs it: decides each commit (see
/// `CommitHandler`), then chunks its schedule, gives the chunks heights and
/// checkpoint sequence numbers (the reference's `CheckpointQueue`), and
/// builds the prologue transaction; and emits the results. Like
/// `CommitHandler`, it reads only its input and state it wrote itself.
///
/// Not here yet, as the design leaves them for later:
/// - deferral and cancellation: the prologue lists no cancelled
///   transactions, and execution time estimates budget nothing (they only
///   go into the prologue's additional state digest);
/// - randomness: transactions using it are scheduled last in the commit's
///   chunks, not in chunks of their own;
/// - the end of the epoch, beyond a stand-in: the first commit at or past the
///   epoch's end time is its last, ending it at once, without draining
///   deferred transactions or a close deadline; a commit's timestamp is not
///   clamped to the epoch's start.
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
    epoch: Arc<EpochParams>,
    handler: CommitHandler,
    start: EpochStart,
    queue: CheckpointQueue,
    commit_times: CommitIntervalObserver,
    /// Commits through this one were decided before a restart: replayed,
    /// they only rebuild the commit interval observer, which is in memory,
    /// as the reference's prior commits do.
    prior_through: u32,
    /// The last commit decided. Accumulator versions count commits, so
    /// commits must come without gaps.
    last_commit: Option<u32>,
    /// The epoch's last commit is decided: later ones schedule nothing.
    closed: bool,
    /// The accumulator root's version the next chunk reads. Each chunk's
    /// barrier, its root's one exclusive writer and with the root its only
    /// input, writes the next version.
    root_version: u64,
    chunks_out: workqueue::Queue<Chunk>,
    settlements_out: workqueue::Queue<SettlementInput>,
    checkpoints_out: workqueue::Queue<waiter::Command<ChunkSettled, PendingCheckpoint>>,
    statuses: Arc<ConsensusTxStatusCache>,
    backpressure: Option<Arc<Backpressure>>,
}

impl CommitStage {
    pub fn new(
        epoch: Arc<EpochParams>,
        start: EpochStart,
        output: ConsensusOutput,
        chunks: workqueue::Queue<Chunk>,
        settlements: workqueue::Queue<SettlementInput>,
        checkpoints: workqueue::Queue<waiter::Command<ChunkSettled, PendingCheckpoint>>,
        statuses: Arc<ConsensusTxStatusCache>,
        committee: Vec<([u8; 96], u64)>,
    ) -> CommitStage {
        // From the last commit written, if any; else the epoch's start.
        let record = output.record().map(|bytes| CommitRecord::decode(&bytes));
        let queue = match record {
            Some(r) => CheckpointQueue::resumed(r.height, r.next_seq, r.last_built_timestamp),
            None => CheckpointQueue::new(start.previous_checkpoint),
        };
        let prior_through = record.map_or(0, |r| r.commit_index);
        let handler = CommitHandler::new(
            output,
            prior_through,
            committee,
            &epoch.config,
            start.authenticator_initial_shared_version,
        );
        CommitStage {
            epoch,
            handler,
            start,
            queue,
            commit_times: CommitIntervalObserver::default(),
            prior_through,
            last_commit: record.map(|r| r.commit_index),
            closed: record.is_some_and(|r| r.closed),
            root_version: record.map_or(start.accumulator_root_version, |r| r.root_version),
            chunks_out: chunks,
            settlements_out: settlements,
            checkpoints_out: checkpoints,
            statuses,
            backpressure: None,
        }
    }

    /// Answers each commit through `backpressure`, which may withhold it.
    #[must_use]
    pub fn with_backpressure(mut self, backpressure: Arc<Backpressure>) -> CommitStage {
        self.backpressure = Some(backpressure);
        self
    }

    /// Decides `commit` and lays it out in chunks and checkpoints, or
    /// nothing if it was handled before or follows the epoch's last.
    pub fn decide<'b>(&mut self, commit: DecodedCommit, bump: &'b Bump) -> Option<Decided<'b>> {
        let epoch = &self.epoch.clone();
        if commit.commit_index <= self.prior_through {
            self.commit_times.observe(
                epoch
                    .config
                    .get_consensus_commit_rate_estimation_window_size(),
                commit.timestamp_ms,
            );
            return None;
        }
        if self.closed {
            return None;
        }
        let HandledCommit {
            mut schedule,
            origins,
            dropped,
            estimates_us,
            ..
        } = self.handler.handle(epoch, commit, bump)?;
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
        //    this one and the estimates of the transactions it schedules.
        self.commit_times.observe(
            config.get_consensus_commit_rate_estimation_window_size(),
            prologue.timestamp_ms,
        );
        let digest = self
            .commit_times
            .digest(&indirect_state_digest(estimates_us));
        let mut prologue_transaction = Some(prologue_transaction(&prologue, &digest));

        // 2. Each transaction with its shared accesses, in schedule order.
        let transactions =
            std::mem::replace(&mut schedule.transactions, containers::Vec::new_in(bump));
        let mut entries = containers::Vec::with_capacity_in(transactions.len(), bump);
        let mut digests = containers::Vec::with_capacity_in(transactions.len(), bump);
        for (i, scheduled) in transactions.into_iter().enumerate() {
            let (transaction, digest, origin) = match scheduled {
                Scheduled::Prologue(_) => {
                    let system = prologue_transaction.take().expect("one prologue");
                    let digest = system.digest;
                    (ExecutableTransaction::System(system), digest, None)
                }
                Scheduled::AuthenticatorStateUpdate(keys) => {
                    let system = authenticator_state_update_transaction(
                        &prologue,
                        self.start
                            .authenticator_initial_shared_version
                            .expect("the authenticator state exists"),
                        keys,
                    );
                    let digest = system.digest;
                    (ExecutableTransaction::System(system), digest, None)
                }
                Scheduled::User(transaction) => {
                    let digest = *transaction.get().0.digest();
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

        // 3. The epoch's last commit reserves the change-epoch transaction's
        //    write of the system state, after every other.
        let end = self
            .start
            .epoch_start_timestamp_ms
            .saturating_add(self.start.epoch_duration_ms);
        let close = (prologue.timestamp_ms >= end).then(|| {
            self.closed = true;
            let writes_before = entries
                .iter()
                .filter(|e| {
                    e.shared.iter().any(|a| {
                        a.id == SYSTEM_STATE
                            && a.mutability
                                == messages::transaction::SharedObjectMutability::Mutable
                    })
                })
                .count();
            let mut close = vec![SharedAccess {
                id: SYSTEM_STATE,
                initial_shared_version: SYSTEM_STATE_INITIAL_SHARED_VERSION,
                mutability: messages::transaction::SharedObjectMutability::Mutable,
                ordinal: SharedOrdinal {
                    round: prologue.round,
                    writes_before: u32::try_from(writes_before)
                        .expect("fewer than 2^32 transactions"),
                },
                base: None,
            }];
            self.handler.link(&mut close);
            close
        });

        // 4. Chunks and checkpoints.
        let mut decided = self.chunk(epoch, &prologue, entries, digests, close, bump);
        decided.dropped = dropped;
        Some(decided)
    }

    /// Chunks of at most `max_transactions_per_checkpoint`, each with the
    /// next height, queued into checkpoints: the reference's
    /// `create_pending_checkpoints`.
    fn chunk<'b>(
        &mut self,
        epoch: &Arc<EpochParams>,
        prologue: &Prologue,
        entries: containers::Vec<'b, ScheduledEntry>,
        digests: containers::Vec<'b, TxDigest>,
        close: Option<Vec<SharedAccess>>,
        bump: &'b Bump,
    ) -> Decided<'b> {
        let config = &epoch.config;
        let max = usize::try_from(config.max_transactions_per_checkpoint())
            .expect("a checkpoint's size fits in usize");
        let chunk_count = entries.len().div_ceil(max);
        let mut decided = Decided {
            chunks: containers::Vec::with_capacity_in(chunk_count, bump),
            settlements: containers::Vec::with_capacity_in(chunk_count, bump),
            checkpoints: containers::Vec::new_in(bump),
            dropped: containers::Vec::new_in(bump),
        };
        let mut flushed = containers::Vec::new_in(bump);
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
            let mut root = [SharedAccess {
                id: ACCUMULATOR_ROOT,
                initial_shared_version: self.start.accumulator_root_initial_shared_version,
                mutability: messages::transaction::SharedObjectMutability::Mutable,
                ordinal: SharedOrdinal {
                    round: prologue.round,
                    writes_before: u32::try_from(position).expect("fewer than 2^32 chunks"),
                },
                base: None,
            }];
            // Linked to the root's last generation in an earlier round.
            self.handler.link(&mut root);
            decided.settlements.push(ChunkSettlement {
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
                root: root[0],
            });
            decided.chunks.push(Chunk {
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
        if close.is_some() {
            flushed.extend(self.queue.flush_forced());
        }
        decided
            .checkpoints
            .extend(flushed.into_iter().map(|f| PendingCheckpoint {
                seq: f.seq,
                heights: f.heights,
                timestamp_ms: f.timestamp_ms,
                close: None,
            }));
        if let Some(shared) = close {
            let last = decided
                .checkpoints
                .last_mut()
                .expect("the last commit cuts the epoch's last checkpoint");
            last.close = Some(EpochClose {
                version: AccumulatorVersion(u64::from(prologue.commit_index)),
                round: prologue.round,
                root_version: self.root_version,
                shared,
                first_checkpoint: self.start.previous_checkpoint + 1,
                observations: self.handler.execution_time_observations(),
            });
        }
        decided
    }

    fn emit(&mut self, decided: Decided<'_>, bump: &Bump) {
        // Dropped transactions go nowhere: no processor waits on them yet.
        let Decided {
            chunks,
            settlements,
            checkpoints,
            dropped: _,
        } = decided;
        for chunk in chunks {
            if let Some(backpressure) = &self.backpressure {
                backpressure.scheduled(chunk.height, chunk.entries.len());
            }
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
                let mut heights = containers::Vec::with_capacity_in(checkpoint.heights.len(), bump);
                heights.extend(checkpoint.heights.iter().map(|&h| ChunkSettled(h)));
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
    type Input = DecodedCommit;
    type Output = ();

    /// Handles `commit`, then answers it: consensus may send the next.
    fn process(&mut self, mut commit: DecodedCommit, bump: &mut Bump) -> Option<()> {
        let handled = commit.handled.take();
        let decided = self.handle(commit, bump);
        if let Some(handled) = handled {
            match &self.backpressure {
                Some(backpressure) => backpressure.answer(handled),
                // Consensus may have stopped waiting; nothing to do then.
                None => {
                    let _ = handled.send(());
                }
            }
        }
        decided
    }
}

impl CommitStage {
    fn handle(&mut self, mut commit: DecodedCommit, bump: &Bump) -> Option<()> {
        let leader_round = commit.leader.round;
        let rejected = std::mem::take(&mut commit.rejected);
        let mut positions = containers::Vec::with_capacity_in(commit.transactions.len(), bump);
        positions.extend(commit.transactions.iter().map(|t| (t.block, t.index)));
        let commit_index = commit.commit_index;
        let decided = self.decide(commit, bump)?;
        let record = CommitRecord {
            commit_index,
            leader_round,
            height: self.queue.height,
            next_seq: self.queue.next_seq,
            last_built_timestamp: self.queue.last_built_timestamp,
            root_version: self.root_version,
            closed: self.closed,
        };
        self.handler
            .finish(&record, self.queue.height, self.queue.pending.is_empty());
        // What became of each transaction, for its submitter: dropped, or
        // finalized (scheduled, or a duplicate of one that was).
        let mut dropped = containers::hash_set(bump, decided.dropped.len());
        dropped.extend(decided.dropped.iter().map(|(b, i, ..)| (*b, *i)));
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
            bump,
        );
        self.emit(decided, bump);
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
    /// No checkpoint cut yet this epoch: as the reference's, its first
    /// commit cuts one, whatever the interval.
    fn new(previous_checkpoint: u64) -> CheckpointQueue {
        CheckpointQueue {
            last_built_timestamp: 0,
            pending: Vec::new(),
            pending_timestamp: 0,
            pending_transactions: 0,
            height: 0,
            next_seq: previous_checkpoint + 1,
        }
    }

    /// The queue as a commit that left nothing pending left it.
    fn resumed(height: u64, next_seq: u64, last_built_timestamp: u64) -> CheckpointQueue {
        CheckpointQueue {
            last_built_timestamp,
            pending: Vec::new(),
            pending_timestamp: 0,
            pending_transactions: 0,
            height,
            next_seq,
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
        flushed: &mut containers::Vec<'_, Flushed>,
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

    /// The reference's `AdditionalConsensusState::digest`, folded with the
    /// commit's `indirect` state digest by `IndirectStateObserver::fold_with`.
    fn digest(&self, indirect: &[u8; 32]) -> [u8; 32] {
        let state = Blake2b::<U32>::digest(
            bcs::to_bytes(&self.timestamps).expect("a sequence of u64 serializes"),
        );
        let mut fold = Blake2b::<U32>::new();
        // Each digest as BCS: its length, then its bytes.
        fold.update([32]);
        fold.update(state);
        fold.update([32]);
        fold.update(indirect);
        fold.finalize().into()
    }
}

/// The reference's `IndirectStateObserver` digest: what it observed of a
/// commit, each scheduled user transaction's execution time estimate, as
/// BCS of `Some(estimate_us)`.
fn indirect_state_digest(estimates_us: impl IntoIterator<Item = u64>) -> [u8; 32] {
    let mut hash = Blake2b::<U32>::new();
    for estimate in estimates_us {
        hash.update([1]);
        hash.update(estimate.to_le_bytes());
    }
    hash.finalize().into()
}

/// `prologue`'s `ConsensusCommitPrologueV4`, as the reference's
/// `create_consensus_commit_prologue_transaction`: no cancelled transactions,
/// and `additional_state_digest` over the commits through this one.
pub fn prologue_transaction(
    prologue: &Prologue,
    additional_state_digest: &[u8; 32],
) -> SystemTransaction {
    use messages::build::base::{AdditionalConsensusStateDigest, ConsensusCommitDigest, Digest};
    use messages::build::system_transaction::{
        ConsensusCommitPrologueV4, ConsensusDeterminedVersionAssignments,
    };
    use messages::build::transaction::TransactionKind;

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
    system_transaction(
        kind,
        SystemKind::Prologue {
            round: prologue.round,
        },
    )
}

/// The `AuthenticatorStateUpdate` of the commit `prologue` begins, adding
/// `keys`, as the reference's `authenticator_state_update_transaction`: in
/// the order of `jwk_lt`, as validated this epoch.
pub fn authenticator_state_update_transaction(
    prologue: &Prologue,
    initial_shared_version: u64,
    mut keys: Vec<(JwkId, Jwk)>,
) -> SystemTransaction {
    use messages::build::base::SequenceNumber;
    use messages::build::system_transaction::{ActiveJwk, AuthenticatorStateUpdate};
    use messages::build::transaction::TransactionKind;

    keys.sort_by(jwk_order);
    let kind = TransactionKind::AuthenticatorStateUpdate(AuthenticatorStateUpdate {
        epoch: prologue.epoch,
        round: u64::from(prologue.round),
        new_active_jwks: keys
            .into_iter()
            .map(|(jwk_id, jwk)| ActiveJwk {
                jwk_id,
                jwk,
                epoch: prologue.epoch,
            })
            .collect(),
        authenticator_obj_initial_shared_version: SequenceNumber(initial_shared_version),
    });
    system_transaction(
        kind,
        SystemKind::AuthenticatorStateUpdate {
            round: prologue.round,
        },
    )
}

/// The transaction ending `epoch`: an `EndOfEpochTransaction` of `kinds`,
/// then `ChangeEpoch`, starting epoch `epoch + 1` at
/// `epoch_start_timestamp_ms` with `protocol_version` and no new system
/// packages, and charging the epoch's gas totals `gas` (computation,
/// storage, rebate, non-refundable). The reference's
/// `create_and_execute_advance_epoch_tx`; `kinds` are what it puts before
/// `ChangeEpoch`.
pub fn end_of_epoch_transaction(
    epoch: u64,
    mut kinds: Vec<messages::build::system_transaction::EndOfEpochTransactionKind>,
    protocol_version: u64,
    gas: [u64; 4],
    epoch_start_timestamp_ms: u64,
) -> SystemTransaction {
    use messages::build::base::ProtocolVersion;
    use messages::build::system_transaction::{ChangeEpoch, EndOfEpochTransactionKind};
    use messages::build::transaction::TransactionKind;

    let [computation, storage, rebate, non_refundable] = gas;
    kinds.push(EndOfEpochTransactionKind::ChangeEpoch(ChangeEpoch {
        epoch: epoch + 1,
        protocol_version: ProtocolVersion(protocol_version),
        storage_charge: storage,
        computation_charge: computation,
        storage_rebate: rebate,
        non_refundable_storage_fee: non_refundable,
        epoch_start_timestamp_ms,
        system_packages: Vec::new(),
    }));
    system_transaction(
        TransactionKind::EndOfEpochTransaction(kinds),
        SystemKind::ChangeEpoch { epoch },
    )
}

/// A system transaction of `kind`: sent by the zero address, with no gas
/// and an all-zero signature, as the reference's `VerifiedTransaction::new_system_transaction`.
fn system_transaction(
    kind: messages::build::transaction::TransactionKind,
    system: SystemKind,
) -> SystemTransaction {
    use messages::build::base::{
        AccountAddress, Digest, ObjectDigest, ObjectId, SequenceNumber, SuiAddress,
    };
    use messages::build::signature::{EmptySignInfo, GenericSignature};
    use messages::build::transaction::{
        GasData, Intent, IntentMessage, SenderSignedData, SenderSignedTransaction, Transaction,
        TransactionData, TransactionDataV1, TransactionExpiration,
    };
    /// `GAS_PRICE_FOR_SYSTEM_TX`.
    const SYSTEM_GAS_PRICE: u64 = 1;

    let sender = SuiAddress([0; 32]);
    let data = TransactionData::V1(TransactionDataV1 {
        kind,
        sender,
        gas_data: GasData {
            payment: vec![(
                ObjectId(AccountAddress([0; 32])),
                SequenceNumber(0),
                ObjectDigest(Digest([0; 32])),
            )],
            owner: sender,
            price: SYSTEM_GAS_PRICE,
            budget: 0,
        },
        expiration: TransactionExpiration::None,
    });
    let size = bcs::serialized_size(&data).expect("transaction data serializes");
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
    let bytes = bcs::to_bytes(&transaction).expect("a transaction serializes");
    // The data follows the one-element sequence's length and the intent.
    let digest = Hashed::of("TransactionData", &bytes[4..4 + size]);
    SystemTransaction {
        kind: system,
        bytes,
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
        let mut queue = CheckpointQueue::new(0);
        let bump = Bump::default();
        let mut flushed = containers::Vec::new_in(&bump);
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
        let mut queue = CheckpointQueue::new(7);
        let bump = Bump::default();
        let mut flushed = containers::Vec::new_in(&bump);
        let height = queue.next_height();
        assert_eq!(queue.push_chunk(height, 9, 0, 5, &mut flushed), 8);
        assert!(flushed.is_empty());
        let height = queue.next_height();
        assert_eq!(queue.push_chunk(height, 1, 0, 5, &mut flushed), 9);
        assert_eq!(heights(&flushed), vec![(8, vec![1], 0)]);
    }

    #[test]
    fn checkpoints_are_cut_once_the_interval_has_passed_by_commit_time() {
        let mut queue = CheckpointQueue::new(0);
        let bump = Bump::default();
        let mut flushed = containers::Vec::new_in(&bump);
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
        let none = indirect_state_digest([]);
        assert_eq!(long.digest(&none), short.digest(&none));
        short.observe(3, 5000);
        assert_ne!(long.digest(&none), short.digest(&none));
    }
}
