// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The work items that flow along the pipeline's queues and wait in its
//! waiters. Each names its producer and consumer.

use std::sync::Arc;
use std::time::Duration;

use ::consensus::{BlockRef, TransactionIndex};
use messages::Message;
use messages::base::ObjectId;
use messages::object::Object;

use crate::checks::VerifiedTransaction;
use crate::consensus::execution_time::Key;
use crate::consensus::sequence::SharedAccess;
use crate::pipeline::keys::{AccumulatorVersion, Reservation, StreamId, TxDigest};
use crate::pipeline::tables::{BuiltCheckpoint, Generation};

/// A transaction the pipeline executes: a user transaction from consensus,
/// or a system transaction the pipeline builds.
// Nearly every transaction is a user transaction: boxing it would cost an
// allocation each to save space on the rare system one.
#[derive(Debug)]
pub enum ExecutableTransaction {
    User(VerifiedTransaction),
    System(SystemTransaction),
}

/// A system transaction built by the commit handler (the prologue) or the
/// settlement scheduler (settlements, the barrier).
#[derive(Debug, Clone)]
pub struct SystemTransaction {
    pub kind: SystemKind,
    /// `Transaction` BCS.
    pub bytes: Vec<u8>,
    pub digest: TxDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKind {
    /// The consensus commit prologue of the commit in `round`.
    Prologue { round: u32 },
    /// The authenticator state's update in the commit in `round`.
    AuthenticatorStateUpdate { round: u32 },
    /// The `index`th settlement transaction of the chunk at `height`.
    Settlement { height: u64, index: u32 },
    /// The accumulator barrier of the chunk at `height`.
    Barrier { height: u64 },
    /// The change-epoch transaction ending `epoch`: the last of its last
    /// checkpoint.
    ChangeEpoch { epoch: u64 },
}

/// A scheduled transaction with its shared accesses and, for a user
/// transaction, where consensus put it.
#[derive(Debug)]
pub struct ScheduledEntry {
    pub transaction: ExecutableTransaction,
    pub shared: Vec<SharedAccess>,
    pub origin: Option<(BlockRef, TransactionIndex)>,
    /// A transaction cancelled for congestion: each consensus object input
    /// and the marker version it reads as instead (then `shared` is empty).
    pub cancelled: Vec<(ObjectId, u64)>,
}

/// A checkpoint chunk's transactions, in schedule order.
/// `CommitHandler` → `VersionResolver`.
#[derive(Debug)]
pub struct Chunk {
    /// The version the commit's transactions reserve funds against.
    pub version: AccumulatorVersion,
    /// The accumulator root's version the chunk's transactions are assigned
    /// (the reference's `SystemObjectVersions`): the one the previous
    /// chunk's barrier wrote, which its settlement reads.
    pub root_version: u64,
    pub round: u32,
    pub height: u64,
    pub checkpoint_seq: u64,
    pub entries: Vec<ScheduledEntry>,
}

/// What settling a chunk takes. `CommitHandler` → `SettlementScheduler`.
#[derive(Debug, Clone)]
pub struct ChunkSettlement {
    /// The version the commit's transactions reserve funds against; settling
    /// the commit's last chunk makes `version + 1`.
    pub version: AccumulatorVersion,
    /// As `Chunk::root_version`; the barrier writes `root_version + 1`.
    pub root_version: u64,
    pub last_of_commit: bool,
    pub round: u32,
    pub height: u64,
    pub checkpoint_seq: u64,
    /// The chunk's transactions' digests, in schedule order.
    pub digests: Vec<TxDigest>,
    /// The accumulator root generation the chunk's settlement reads: its
    /// settlements write it non-exclusively, its barrier exclusively.
    pub root: SharedAccess,
}

/// What `SettlementScheduler` takes: a chunk to settle, from `CommitStage`,
/// or the effects it waited for, from the effects waiter.
#[derive(Debug)]
pub enum SettlementInput {
    Chunk(ChunkSettlement),
    /// A chunk's transactions executed.
    ChunkExecuted(ChunkSettlement),
    /// A chunk's settlement and barrier transactions executed.
    SettlementExecuted {
        height: u64,
        digests: Vec<TxDigest>,
    },
}

/// A checkpoint waiting for its chunks. `CommitHandler` → checkpoint waiter
/// (wait for `ChunkSettled` of each height) → `ContentsBuilder`.
#[derive(Debug, Clone)]
pub struct PendingCheckpoint {
    pub seq: u64,
    /// Its chunks' heights, in order.
    pub heights: Vec<u64>,
    pub timestamp_ms: u64,
    /// Set on the epoch's last checkpoint only.
    pub close: Option<EpochClose>,
}

/// What the commit handler reserved, in the epoch's last commit, for the
/// change-epoch transaction that `EpochCloser` builds once everything else
/// in the epoch has executed.
#[derive(Debug, Clone)]
pub struct EpochClose {
    /// As the last commit's chunks'.
    pub version: AccumulatorVersion,
    pub round: u32,
    /// The accumulator root's version after the last chunk's barrier.
    pub root_version: u64,
    /// Its shared accesses, after every other access of the epoch.
    pub shared: Vec<SharedAccess>,
    /// The epoch's first checkpoint.
    pub first_checkpoint: u64,
    /// The execution time observations as of the last commit, by key and
    /// authority name, sorted by key: the next epoch's, once filtered.
    pub observations: Vec<(Key, Vec<([u8; 96], Duration)>)>,
    /// The next epoch's protocol version, and the system packages to go
    /// with it, as the last commit chose them (`capabilities::choose`).
    pub protocol_version: u64,
    pub system_packages: Vec<messages::build::base::ObjectRef>,
}

/// A transaction with its inputs as keys. `VersionResolver` or
/// `SettlementScheduler` → input waiter → `Executor`.
#[derive(Debug)]
pub struct Executable {
    /// The version its commit reserves funds against.
    pub version: AccumulatorVersion,
    /// As `Chunk::root_version`.
    pub root_version: u64,
    /// Its funds reservations, if any: it then also waits on
    /// `InputKey::Reserved`, and runs only if all were fulfilled (else it
    /// fails without running, as the reference's
    /// `FundsWithdrawStatus::Insufficient`). What it withdraws short of each
    /// is credited back.
    pub reserved: Vec<Reservation>,
    pub height: u64,
    pub entry: ScheduledEntry,
    /// Each shared access's generation 0 resolved to the earlier generation
    /// it reads, where the resolver knew one: (stream, round, generation).
    pub resolved_base: Vec<(StreamId, u32, u32)>,
    /// What each of its input waiter keys resolved to, by the key's position:
    /// filled in by the input waiter.
    pub inputs: Vec<Option<InputValue>>,
}

/// What the input waiter takes: transactions waiting on their inputs, and
/// keys resolved to their values.
pub type InputCommand = waiter::Command<crate::pipeline::keys::InputKey, Executable, InputValue>;

/// What an input waiter key resolves to.
#[derive(Clone, Debug)]
pub enum InputValue {
    /// `ObjectVersion` or `Package`: the object.
    Object(Message<Object<'static>>),
    /// `Generation`: the generation, and the object at its version unless
    /// the stream ended.
    Generation(Generation, Option<Message<Object<'static>>>),
    /// `StreamLive`: nothing. `Executor` reads generation 0 itself, as it
    /// must be the same for every transaction of the round.
    Live,
    /// `Reserved`: how the transaction's funds reservations resolved.
    Reserved(ReservationStatus),
}

impl waiter::Receive<InputValue> for Executable {
    fn receive(&mut self, index: usize, value: InputValue) {
        if self.inputs.len() <= index {
            self.inputs.resize(index + 1, None);
        }
        self.inputs[index] = Some(value);
    }
}

/// A transaction's funds reservations, to resolve. `VersionResolver` →
/// `FundsReserver`, while the transaction waits in the input waiter on
/// `InputKey::Reserved(digest)`.
#[derive(Debug, Clone)]
pub struct Withdrawal {
    pub digest: TxDigest,
    /// One per account, in account order, all against the transaction's
    /// version.
    pub reservations: Vec<Reservation>,
}

/// How a reservation resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationStatus {
    Fulfilled,
    Cancelled,
}

/// What `FundsReserver` learns about balances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    /// A transaction deposited `amount` into `account`, settling into
    /// `version`. `Executor` → `FundsReserver`.
    Deposit(ObjectId, u64, AccumulatorVersion),
    /// A fulfilled reservation against `version` withdrew `amount` less
    /// from `account` than it reserved. `Executor` → `FundsReserver`.
    Credit(ObjectId, u64, AccumulatorVersion),
    /// `version` settled: the exact balance of each account its settlement
    /// changed, and the root's version, at or below which every account's
    /// version at `version` lies. `SettlementScheduler` → `FundsReserver`.
    Settled {
        balances: Vec<(ObjectId, u128)>,
        version: AccumulatorVersion,
        root_version: u64,
    },
}

/// `FundsReserver`'s input: transactions to wait on their reservations, and
/// what resolves them.
#[derive(Debug)]
pub enum FundsCommand {
    Reserve(Vec<Withdrawal>),
    Settle(Vec<Settle>),
}

/// What waits for effects. The effects waiter routes a ready item by kind.
pub enum EffectsWaitItem {
    /// A wait for one transaction's effects, answered once they are in the
    /// store. The RPC handler's `wait_for_effects` → effects waiter → reply.
    Executed(tokio::sync::oneshot::Sender<()>),
    /// A chunk's transactions executed. `SettlementScheduler` → effects
    /// waiter → `SettlementScheduler`.
    ChunkExecuted(ChunkSettlement),
    /// A chunk's settlement and barrier transactions executed.
    /// `SettlementScheduler` → effects waiter → `SettlementScheduler`.
    SettlementExecuted { height: u64, digests: Vec<TxDigest> },
    /// The change-epoch transaction executed. `EpochCloser` → effects waiter
    /// → `EpochCloser`.
    ChangeEpochExecuted,
}

/// A transaction's executed effects, as the chunk table and checkpointing
/// keep them.
#[derive(Debug, Clone)]
pub struct ExecutedEffects {
    pub digest: TxDigest,
    pub effects_digest: TxDigest,
    /// `TransactionEffects` BCS.
    pub effects: Arc<[u8]>,
}

/// A settled chunk: its effects in checkpoint order (causal order, the
/// prologue first, then settlements and the barrier).
/// `SettlementScheduler` → chunk table → `ContentsBuilder`.
#[derive(Debug, Clone)]
pub struct SettledChunk {
    pub height: u64,
    pub checkpoint_seq: u64,
    pub effects: Vec<ExecutedEffects>,
}

/// A checkpoint's effects, for its state hash. `ContentsBuilder`, or
/// `EpochCloser` for the epoch's last → `StateHasher`.
#[derive(Debug, Clone)]
pub struct CheckpointEffects {
    pub seq: u64,
    pub effects: Vec<ExecutedEffects>,
    /// The epoch's last checkpoint: its root goes to `EpochCloser`.
    pub last_of_epoch: bool,
}

/// A checkpoint's contents, built without the previous checkpoint.
/// `ContentsBuilder` → built waiter (wait for `CheckpointBuilt(seq - 1)`) →
/// `EpochCloser` → `SummaryBuilder`.
#[derive(Debug, Clone)]
pub struct CheckpointContentsBuilt {
    pub pending: PendingCheckpoint,
    /// `CheckpointContents` BCS and digest.
    pub contents: Arc<[u8]>,
    pub contents_digest: [u8; 32],
    /// This checkpoint's gas cost: computation, storage, rebate, non-refundable.
    pub gas: [u64; 4],
    /// `[CheckpointArtifacts digest]`, when the protocol includes it.
    pub artifacts_digest: Option<[u8; 32]>,
    pub transaction_count: u64,
    /// The rounds of the checkpoint's randomness state updates, in order,
    /// for the summary's version-specific data.
    pub randomness_rounds: Vec<u64>,
    /// The epoch's last checkpoint: built first without the change-epoch
    /// transaction, with its effects kept for `EpochCloser` to build it
    /// again with it; then with its end-of-epoch data.
    pub last: Option<LastOfEpoch>,
}

/// What `EpochCloser` takes.
#[derive(Debug)]
pub enum EpochCloserInput {
    /// Checkpoints whose previous one is built. Built waiter →
    /// `EpochCloser`.
    Built(Vec<CheckpointContentsBuilt>),
    /// The change-epoch transaction executed. Effects waiter →
    /// `EpochCloser`.
    ChangeEpochExecuted,
    /// The live object set's ECMH after the epoch's last checkpoint
    /// (`StateHash` BCS). `StateHasher` → `EpochCloser`.
    Root(Vec<u8>),
}

/// The epoch's last checkpoint, on its way through `EpochCloser`.
#[derive(Debug, Clone)]
pub enum LastOfEpoch {
    /// Its effects, without the change-epoch transaction's.
    Effects(Vec<ExecutedEffects>),
    /// Built whole: its end-of-epoch data.
    Closed(EndOfEpoch),
}

/// A last checkpoint's end-of-epoch data, from the system state the
/// change-epoch transaction wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndOfEpoch {
    /// Each member's protocol key and voting power, in committee order.
    pub next_committee: Vec<([u8; 96], u64)>,
    pub next_protocol_version: u64,
    /// The live object set's ECMH after the checkpoint (`StateHash` BCS),
    /// which the next epoch's root continues from.
    pub root_state: Vec<u8>,
    /// Its digest, the epoch's commitment, when the protocol commits it.
    pub root_state_digest: Option<[u8; 32]>,
}

/// A checkpoint signature from consensus. `BlockVoter` and `CommitDecoder`
/// → built waiter (wait for `CheckpointBuilt(seq)`) → `CheckpointAggregator`.
#[derive(Debug, Clone)]
pub struct CheckpointSignature {
    pub seq: u64,
    /// `CheckpointSignatureMessage` BCS.
    pub message: Arc<[u8]>,
}

/// What waits for this validator to build a checkpoint, keyed by
/// `CheckpointBuilt(seq)` and resolved to the checkpoint built. The built
/// waiter routes a ready item by kind.
#[derive(Debug)]
pub enum BuiltWaitItem {
    /// A checkpoint's contents, waiting for the previous checkpoint.
    /// `ContentsBuilder` → built waiter → `EpochCloser`.
    Next(CheckpointContentsBuilt),
    /// A signature, waiting for the summary it signs. `BlockVoter` and
    /// `CommitDecoder` → built waiter → `CheckpointAggregator`.
    Signature(CheckpointSignature),
    /// A certified checkpoint, waiting for the local one to compare it
    /// with. `CheckpointAggregator` → built waiter → fork check.
    Certified {
        certified: CertifiedCheckpoint,
        local: Option<Arc<BuiltCheckpoint>>,
    },
}

impl waiter::Receive<Arc<BuiltCheckpoint>> for BuiltWaitItem {
    fn receive(&mut self, _: usize, built: Arc<BuiltCheckpoint>) {
        if let BuiltWaitItem::Certified { local, .. } = self {
            *local = Some(built);
        }
    }
}

/// What the built waiter takes.
pub type BuiltCommand =
    waiter::Command<crate::pipeline::keys::CheckpointBuilt, BuiltWaitItem, Arc<BuiltCheckpoint>>;

/// The built waiter's queue.
pub type BuiltWaits = workqueue::Queue<BuiltCommand>;

/// A certified checkpoint. `CheckpointAggregator` → state sync, and →
/// `CheckpointCommitter`, in sequence order.
#[derive(Debug, Clone)]
pub struct CertifiedCheckpoint {
    pub seq: u64,
    /// `CertifiedCheckpointSummary` BCS.
    pub summary: Arc<[u8]>,
}
