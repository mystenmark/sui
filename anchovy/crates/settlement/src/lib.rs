// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Settling each checkpoint chunk, and reserving funds withdrawals
//! (`docs/consensus_pipeline.md`, Settlement).

pub mod causal;
pub mod funds;
pub mod settle;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use containers::{Bump, HeapHashMap};

use messages::Message;
use messages::base::{Hashed, ObjectId};
use messages::effects::{
    AccumulatorValue, ObjectIn, ObjectOut, TransactionEffects, VersionedEffects,
};
use messages::execution_status::ExecutionStatus;
use messages::object::Data;
use messages::transaction::{SharedObjectMutability, TransactionKind};
use validator::consensus::sequence::SharedAccess;
use validator::epoch::EpochParams;
use validator::pipeline::items::ChunkSettlement;
use validator::pipeline::items::{
    EffectsWaitItem, Executable, ExecutableTransaction, ExecutedEffects, FundsCommand,
    InputCommand, InputValue, PendingCheckpoint, ReservationStatus, ScheduledEntry, Settle,
    SettledChunk, SettlementInput, SystemKind, SystemTransaction, Withdrawal,
};
use validator::pipeline::keys::{
    AccumulatorVersion, ChunkSettled, EffectsKey, InputKey, StreamId, TxDigest,
};
use validator::pipeline::tables::ChunkTable;
use waiter::{Command, WaitBatch};
use workqueue::{Processor, Queue};

use crate::funds::{Ledger, Ticket};
use crate::settle::{ACCUMULATOR_ROOT, SettlementBuilder};

type Effects = Message<TransactionEffects<'static>>;

/// A full queue is a sizing bug; a closed one means the pipeline is shutting
/// down.
fn push<W>(queue: &Queue<W>, item: W, to: &str) {
    if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
        panic!("the {to} queue refused work");
    }
}

/// The executed effects of `digest`, which its waiter released: this
/// validator's own.
fn executed(store: &store::Store, digest: &TxDigest) -> Effects {
    let effects_digest = store
        .executed_effects(digest)
        .expect("store read")
        .expect("a released transaction has executed");
    store
        .effects(&effects_digest)
        .expect("store read")
        .expect("executed effects are stored")
}

fn is_success(effects: &TransactionEffects<'_>) -> bool {
    match &effects.version {
        VersionedEffects::V1(v1) => matches!(v1.status, ExecutionStatus::Success),
        VersionedEffects::V2(v2) => matches!(v2.status, ExecutionStatus::Success),
    }
}

/// Whether `effects` mutated the accumulator root.
fn mutates_root(effects: &TransactionEffects<'_>) -> bool {
    let VersionedEffects::V2(v2) = &effects.version else {
        return false;
    };
    v2.changed_objects.iter().any(|c| {
        *c.id == ACCUMULATOR_ROOT
            && matches!(c.input_state, ObjectIn::Exist { .. })
            && matches!(c.output_state, ObjectOut::ObjectWrite(..))
    })
}

fn to_executed(effects: &Effects) -> ExecutedEffects {
    let view = effects.get();
    let digest = match &view.version {
        VersionedEffects::V1(v1) => *v1.transaction_digest,
        VersionedEffects::V2(v2) => *v2.transaction_digest,
    };
    ExecutedEffects {
        // The executor that wrote the effects named the digest it computed.
        digest: Hashed::computed(digest),
        effects_digest: *effects.digest(),
        effects: Arc::from(effects.wire_bytes()),
    }
}

/// Where a chunk is in settling.
enum Stage {
    /// Waiting for its transactions' effects.
    Executing,
    /// Its effects, in checkpoint order, waiting for the earlier chunks to
    /// settle.
    Ordered {
        chunk: ChunkSettlement,
        effects: Vec<Effects>,
    },
    /// Its settlement transactions sent, waiting for their effects.
    Settling {
        chunk: ChunkSettlement,
        effects: Vec<Effects>,
        settlements: Vec<TxDigest>,
    },
    /// Its barrier sent, waiting for its effects.
    Barrier {
        chunk: ChunkSettlement,
        effects: Vec<Effects>,
        settlements: Vec<Effects>,
        barrier: TxDigest,
    },
}

struct Pending {
    /// For a chunk reading the root's generation 0 of its round: the
    /// previous chunk's barrier generation, (round, generation).
    base: Option<(u32, u32)>,
    stage: Stage,
}

/// One chunk at a time, in height order: waits for the chunk's effects,
/// orders them causally (the prologue first, as checkpoints order them),
/// builds the settlement transactions (accumulator root, non-exclusive
/// write) and then, from their effects, the barrier (exclusive write), has
/// them executed, then records the settled chunk and notifies.
///
/// A chunk's effects are ordered as soon as they execute; its settlement
/// waits for the earlier chunks to settle, since its event indices start at
/// its offset in its checkpoint. That offset is the count of effects of the
/// checkpoint's chunks settled before it here: the first chunk this
/// processor sees starts at 0, which holds after a restart because replay
/// starts at a checkpoint boundary.
///
/// Inward:
/// - `CommitStage`: `SettlementInput::Chunk`;
/// - effects waiter, ready (through `EffectsRouter`):
///   `SettlementInput::ChunkExecuted`, `SettlementInput::SettlementExecuted`
///   (once for the settlement transactions, once for the barrier).
///
/// Outward:
/// - effects waiter, wait for: `EffectsWaitItem::ChunkExecuted` keyed by the
///   chunk's transactions, and `EffectsWaitItem::SettlementExecuted` keyed by
///   its settlement transactions, then by its barrier;
/// - input waiter, wait for: the settlement transactions, then the barrier
///   (`Executable`), keyed by the root's generation;
/// - checkpoint waiter, notify: `ChunkSettled(height)`;
/// - `FundsReserver`: `Settle::Settled` of version `v + 1` once the last
///   chunk of commit `v` settled, and of the first chunk's version, whose
///   accumulator state is the one in the store.
///
/// Reads: the object store (effects, the first transaction, to tell a
/// prologue, and the balances settled). Writes: the chunk table.
pub struct SettlementScheduler {
    epoch: Arc<EpochParams>,
    store: Arc<store::Store>,
    chunks: Arc<ChunkTable>,
    effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    inputs: Queue<InputCommand>,
    checkpoints: Queue<Command<ChunkSettled, PendingCheckpoint>>,
    funds: Queue<FundsCommand>,
    /// The accounts the settling commit's chunks changed so far.
    accounts: BTreeSet<ObjectId>,
    /// Chunks not settled yet, by height: the first is the one settling.
    pending: BTreeMap<u64, Pending>,
    /// Whether a chunk was seen: the first tells the funds reserver what is
    /// settled.
    started: bool,
    /// The checkpoint of the last chunk settled, and how many effects its
    /// chunks settled so far hold.
    offset: Option<(u64, u64)>,
}

impl SettlementScheduler {
    pub fn new(
        epoch: Arc<EpochParams>,
        store: Arc<store::Store>,
        chunks: Arc<ChunkTable>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        inputs: Queue<InputCommand>,
        checkpoints: Queue<Command<ChunkSettled, PendingCheckpoint>>,
        funds: Queue<FundsCommand>,
    ) -> SettlementScheduler {
        SettlementScheduler {
            epoch,
            store,
            chunks,
            effects,
            inputs,
            checkpoints,
            funds,
            accounts: BTreeSet::new(),
            pending: BTreeMap::new(),
            started: false,
            offset: None,
        }
    }

    fn chunk(&mut self, chunk: ChunkSettlement, bump: &Bump) {
        if !self.started {
            self.started = true;
            // Every chunk before the first one seen settled before this
            // run: the epoch's start.
            push(
                &self.funds,
                FundsCommand::Settle(vec![Settle::Settled {
                    balances: Vec::new(),
                    version: chunk.version,
                    root_version: chunk.root_version,
                }]),
                "funds reserver",
            );
        }
        // The commit handler links a round's first chunk to the root's last
        // generation in an earlier round.
        let base = match (chunk.root.ordinal.writes_before, chunk.root.base) {
            (0, Some(base)) => Some((base.round, base.writes_before)),
            _ => None,
        };
        self.pending.insert(
            chunk.height,
            Pending {
                base,
                stage: Stage::Executing,
            },
        );
        let mut batch = WaitBatch::with_capacity(1, chunk.digests.len());
        let mut keys = containers::Vec::with_capacity_in(chunk.digests.len(), bump);
        keys.extend(chunk.digests.iter().map(|d| EffectsKey(**d)));
        batch.push(EffectsWaitItem::ChunkExecuted(chunk), keys);
        push(&self.effects, Command::Wait(batch), "effects waiter");
    }

    fn chunk_executed(&mut self, chunk: ChunkSettlement, bump: &mut Bump) {
        let keep = self.epoch.config.disable_effects_tx_dependencies();
        let prologue_first = !keep && self.is_prologue(chunk.digests.first());
        let effects = {
            let bump = &*bump;
            let mut effects = containers::Vec::with_capacity_in(chunk.digests.len(), bump);
            effects.extend(chunk.digests.iter().map(|d| executed(&self.store, d)));
            let mut views = containers::Vec::with_capacity_in(effects.len(), bump);
            views.extend(effects.iter().map(Message::get));
            let order = causal::checkpoint_order(&views, prologue_first, keep, bump);
            drop(views);
            let mut slots = containers::Vec::with_capacity_in(effects.len(), bump);
            slots.extend(effects.into_iter().map(Some));
            order
                .iter()
                .map(|&i| slots[i].take().expect("a permutation"))
                .collect()
        };
        let pending = self
            .pending
            .get_mut(&chunk.height)
            .expect("a chunk executes after it is seen");
        pending.stage = Stage::Ordered { chunk, effects };
        self.advance(bump);
    }

    /// Whether `digest` is a consensus commit prologue, which checkpoint
    /// order keeps first.
    fn is_prologue(&self, digest: Option<&TxDigest>) -> bool {
        let Some(digest) = digest else {
            return false;
        };
        let transaction = self
            .store
            .transaction(digest)
            .expect("store read")
            .expect("an executed transaction is stored");
        matches!(
            transaction.get().data().kind(),
            TransactionKind::ConsensusCommitPrologue(_)
                | TransactionKind::ConsensusCommitPrologueV2(_)
                | TransactionKind::ConsensusCommitPrologueV3(_)
                | TransactionKind::ConsensusCommitPrologueV4(_)
        )
    }

    /// Starts settling the first pending chunk, if its effects are ordered.
    fn advance(&mut self, bump: &Bump) {
        let Some(mut entry) = self.pending.first_entry() else {
            return;
        };
        if !matches!(entry.get().stage, Stage::Ordered { .. }) {
            return;
        }
        let height = *entry.key();
        let base = entry.get().base;
        let Stage::Ordered { chunk, effects } =
            std::mem::replace(&mut entry.get_mut().stage, Stage::Executing)
        else {
            unreachable!()
        };
        let offset = match self.offset {
            Some((seq, count)) if seq == chunk.checkpoint_seq => count,
            _ => 0,
        };
        let mut builder = SettlementBuilder::new(chunk.checkpoint_seq, offset, bump);
        for (i, e) in effects.iter().enumerate() {
            builder.add(i as u64, e.get());
        }
        let kinds = builder.build(
            self.epoch.config.max_updates_per_settlement_txn_as_option(),
            self.epoch.epoch,
            chunk.root.initial_shared_version,
            height,
        );
        if kinds.is_empty() {
            self.send_barrier(height, base, chunk, effects, Vec::new());
            return;
        }
        let mut transactions = WaitBatch::with_capacity(kinds.len(), kinds.len());
        let mut settlements = Vec::with_capacity(kinds.len());
        for (index, kind) in kinds.into_iter().enumerate() {
            let built = settle::system_transaction(kind);
            settlements.push(built.digest);
            let kind = SystemKind::Settlement {
                height,
                index: index as u32,
            };
            let (executable, key) = executable(
                &chunk,
                base,
                kind,
                built,
                SharedObjectMutability::NonExclusiveWrite,
            );
            transactions.push(executable, [key]);
        }
        let mut waits = WaitBatch::with_capacity(1, settlements.len());
        waits.push(
            EffectsWaitItem::SettlementExecuted {
                height,
                digests: settlements.clone(),
            },
            settlements.iter().map(|d| EffectsKey(**d)),
        );
        push(&self.inputs, Command::Wait(transactions), "input waiter");
        push(&self.effects, Command::Wait(waits), "effects waiter");
        self.pending
            .get_mut(&height)
            .expect("the settling chunk is pending")
            .stage = Stage::Settling {
            chunk,
            effects,
            settlements,
        };
    }

    fn send_barrier(
        &mut self,
        height: u64,
        base: Option<(u32, u32)>,
        chunk: ChunkSettlement,
        effects: Vec<Effects>,
        settlements: Vec<Effects>,
    ) {
        let kind = settle::barrier(
            self.epoch.epoch,
            chunk.root.initial_shared_version,
            height,
            settlements.len() as u64,
            settle::count_object_changes(settlements.iter().map(Message::get)),
        );
        let built = settle::system_transaction(kind);
        let barrier = built.digest;
        let (executable, key) = executable(
            &chunk,
            base,
            SystemKind::Barrier { height },
            built,
            SharedObjectMutability::Mutable,
        );
        let mut transactions = WaitBatch::with_capacity(1, 1);
        transactions.push(executable, [key]);
        let mut waits = WaitBatch::with_capacity(1, 1);
        waits.push(
            EffectsWaitItem::SettlementExecuted {
                height,
                digests: vec![barrier],
            },
            [EffectsKey(*barrier)],
        );
        push(&self.inputs, Command::Wait(transactions), "input waiter");
        push(&self.effects, Command::Wait(waits), "effects waiter");
        self.pending
            .get_mut(&height)
            .expect("the settling chunk is pending")
            .stage = Stage::Barrier {
            chunk,
            effects,
            settlements,
            barrier,
        };
    }

    fn settlement_executed(&mut self, height: u64, digests: &[TxDigest], bump: &Bump) {
        let mut entry = self
            .pending
            .first_entry()
            .expect("settlement effects for a pending chunk");
        assert_eq!(*entry.key(), height, "only the first chunk settles");
        let base = entry.get().base;
        match std::mem::replace(&mut entry.get_mut().stage, Stage::Executing) {
            Stage::Settling {
                chunk,
                effects,
                settlements,
            } => {
                assert_eq!(digests, settlements, "the settlements waited for");
                let settlements: Vec<Effects> =
                    digests.iter().map(|d| executed(&self.store, d)).collect();
                self.send_barrier(height, base, chunk, effects, settlements);
            }
            Stage::Barrier {
                chunk,
                effects,
                settlements,
                barrier,
            } => {
                assert_eq!(digests, [barrier], "the barrier waited for");
                entry.remove();
                let barrier = executed(&self.store, &barrier);
                self.settled(&chunk, &effects, &settlements, &barrier);
                self.advance(bump);
            }
            Stage::Executing | Stage::Ordered { .. } => {
                panic!("settlement effects for a chunk not settling")
            }
        }
    }

    /// Records the settled chunk, then notifies its height and the root
    /// version it settled to, each only once what it names is visible.
    fn settled(
        &mut self,
        chunk: &ChunkSettlement,
        effects: &[Effects],
        settlements: &[Effects],
        barrier: &Effects,
    ) {
        let mut root_writes = 0;
        for e in settlements.iter().chain([barrier]) {
            // As the reference asserts, here and in its checkpoint builder.
            assert!(is_success(e.get()), "a settlement transaction failed");
            root_writes += usize::from(mutates_root(e.get()));
        }
        assert_eq!(root_writes, 1, "only the barrier mutates the root");

        let all: Vec<ExecutedEffects> = effects
            .iter()
            .chain(settlements)
            .chain([barrier])
            .map(to_executed)
            .collect();
        let count = all.len() as u64;
        self.chunks.insert(SettledChunk {
            height: chunk.height,
            checkpoint_seq: chunk.checkpoint_seq,
            effects: all,
        });
        push(
            &self.checkpoints,
            Command::notify(vec![ChunkSettled(chunk.height)]),
            "checkpoint waiter",
        );
        // The settlements and the barrier committed the accounts and the
        // root before their effects were notified.
        self.accounts
            .extend(effects.iter().flat_map(|e| accounts(e.get())));
        if chunk.last_of_commit {
            let root_version = lamport_version(barrier.get());
            let balances = std::mem::take(&mut self.accounts)
                .into_iter()
                .map(|account| (account, balance_at(&self.store, &account, root_version)))
                .collect();
            push(
                &self.funds,
                FundsCommand::Settle(vec![Settle::Settled {
                    balances,
                    version: AccumulatorVersion(chunk.version.0 + 1),
                    root_version,
                }]),
                "funds reserver",
            );
        }
        self.offset = Some(match self.offset {
            Some((seq, settled)) if seq == chunk.checkpoint_seq => (seq, settled + count),
            _ => (chunk.checkpoint_seq, count),
        });
    }
}

/// A settlement or barrier transaction, and the input waiter key of the
/// root generation it reads.
fn executable(
    chunk: &ChunkSettlement,
    base: Option<(u32, u32)>,
    kind: SystemKind,
    built: settle::Built,
    mutability: SharedObjectMutability,
) -> (Executable, InputKey) {
    let root = chunk.root;
    let stream = StreamId {
        id: root.id,
        initial_shared_version: root.initial_shared_version,
    };
    let key = match (root.ordinal.writes_before, base) {
        (0, None) => InputKey::StreamLive(stream),
        (0, Some((round, generation))) => InputKey::Generation {
            stream,
            round,
            generation,
        },
        (generation, _) => InputKey::Generation {
            stream,
            round: root.ordinal.round,
            generation,
        },
    };
    let resolved_base = match (root.ordinal.writes_before, base) {
        (0, Some((round, generation))) => vec![(stream, round, generation)],
        _ => Vec::new(),
    };
    let executable = Executable {
        version: chunk.version,
        root_version: chunk.root_version,
        reserved: Vec::new(),
        height: chunk.height,
        entry: ScheduledEntry {
            transaction: ExecutableTransaction::System(SystemTransaction {
                kind,
                bytes: built.bytes,
                digest: built.digest,
            }),
            shared: vec![SharedAccess { mutability, ..root }],
            origin: None,
        },
        resolved_base,
        inputs: Vec::new(),
    };
    (executable, key)
}

impl Processor for SettlementScheduler {
    type Input = SettlementInput;
    type Output = ();

    fn process(&mut self, input: SettlementInput, bump: &mut Bump) -> Option<()> {
        match input {
            SettlementInput::Chunk(chunk) => self.chunk(chunk, bump),
            SettlementInput::ChunkExecuted(chunk) => self.chunk_executed(chunk, bump),
            SettlementInput::SettlementExecuted { height, digests } => {
                self.settlement_executed(height, &digests, bump);
            }
        }
        None
    }
}

/// The balance accounts `effects` write: integer-valued accumulators, not
/// event streams.
fn accounts<'e>(effects: &'e TransactionEffects<'_>) -> impl Iterator<Item = ObjectId> + 'e {
    let changes = match &effects.version {
        VersionedEffects::V2(v2) => v2.changed_objects,
        VersionedEffects::V1(_) => &[],
    };
    changes
        .iter()
        .filter(|c| {
            matches!(&c.output_state, ObjectOut::AccumulatorWriteV1(w)
                if matches!(w.value, AccumulatorValue::Integer(_)))
        })
        .map(|c| *c.id)
}

fn lamport_version(effects: &TransactionEffects<'_>) -> u64 {
    match &effects.version {
        VersionedEffects::V2(v2) => v2.lamport_version,
        VersionedEffects::V1(_) => unreachable!("settlement produces V2 effects"),
    }
}

/// `account`'s balance as of the version whose root is at `root_version`:
/// a settlement writes its accounts at the root's version, and later ones
/// above it.
fn balance_at(store: &store::Store, account: &ObjectId, root_version: u64) -> u128 {
    match store
        .object_at_or_before(account, root_version)
        .expect("store read")
    {
        Some(object) => match &object.get().data {
            Data::Move(o) => funds::account_balance(o.contents),
            Data::Package(_) => panic!("an accumulator account is a package"),
        },
        None => 0,
    }
}

/// A transaction waiting on its reservations.
struct Waiting {
    withdrawal: Withdrawal,
    unresolved: usize,
    /// Each reservation's outcome, once resolved, in `reservations` order.
    resolved: Vec<Option<ReservationStatus>>,
}

/// Resolves funds reservations by the rules of "Funds reservation" in
/// `docs/consensus_pipeline.md` (see `funds::Ledger`). A transaction that
/// withdraws funds waits in the input waiter on `InputKey::Reserved` as well
/// as its inputs; once each of its reservations is `Fulfilled` or
/// `Cancelled`, this resolves the key to how. It runs only if all were
/// fulfilled.
///
/// Inward:
/// - `VersionResolver`: `FundsCommand::Reserve` of each transaction that
///   withdraws funds, in consensus order;
/// - `Executor`: `Settle::Deposit` of each deposit a transaction made, and
///   `Settle::Credit` of what each fulfilled reservation did not withdraw;
/// - `SettlementScheduler`: `Settle::Settled` of each version settled.
///
/// A cancelled transaction withdraws nothing: its fulfilled reservations
/// are credited back here, at once.
///
/// Outward:
/// - input waiter, resolve: `InputKey::Reserved(digest)` of each transaction
///   whose reservations have all resolved, to `InputValue::Reserved`. Its
///   wait is always queued before: the resolver queues it before sending the
///   reservations here.
///
/// Reads: the object store, for the balance at the settled version of an
/// account first seen.
pub struct FundsReserver {
    store: Arc<store::Store>,
    inputs: Queue<InputCommand>,
    ledger: Ledger,
    /// The root's version at the settled version.
    root_version: Option<u64>,
    next_ticket: Ticket,
    /// Each unresolved reservation's transaction, by the transaction's
    /// first ticket, and its index there.
    owners: HeapHashMap<Ticket, (Ticket, usize)>,
    waiting: HeapHashMap<Ticket, Waiting>,
    resolved: Vec<(Ticket, ReservationStatus)>,
}

impl FundsReserver {
    pub fn new(store: Arc<store::Store>, inputs: Queue<InputCommand>) -> Self {
        FundsReserver {
            store,
            inputs,
            ledger: Ledger::new(),
            root_version: None,
            next_ticket: 0,
            owners: HeapHashMap::default(),
            waiting: HeapHashMap::default(),
            resolved: Vec::new(),
        }
    }

    /// Waits `withdrawal` on its reservations.
    fn reserve(&mut self, withdrawal: Withdrawal) {
        let id = self.next_ticket;
        let count = withdrawal.reservations.len();
        assert!(count > 0, "a withdrawal reserves funds");
        let (store, root_version) = (&self.store, self.root_version);
        let mut base = |account: &ObjectId| {
            balance_at(
                store,
                account,
                root_version.expect("read once a version settled"),
            )
        };
        for (index, reservation) in withdrawal.reservations.iter().enumerate() {
            let ticket = self.next_ticket;
            self.next_ticket += 1;
            self.owners.insert(ticket, (id, index));
            self.ledger.reserve(
                ticket,
                reservation.account,
                reservation.amount,
                reservation.version.0,
                &mut base,
                &mut self.resolved,
            );
        }
        self.waiting.insert(
            id,
            Waiting {
                withdrawal,
                unresolved: count,
                resolved: vec![None; count],
            },
        );
    }

    fn settle(&mut self, settle: Settle) {
        if let Settle::Settled { root_version, .. } = &settle {
            self.root_version = Some(*root_version);
        }
        let (store, root_version) = (&self.store, self.root_version);
        let mut base = |account: &ObjectId| {
            balance_at(
                store,
                account,
                root_version.expect("read once a version settled"),
            )
        };
        match settle {
            Settle::Deposit(account, amount, version) => {
                self.ledger
                    .deposit(account, amount, version.0, &mut base, &mut self.resolved);
            }
            Settle::Credit(account, amount, version) => {
                self.ledger
                    .credit(account, amount, version.0, &mut self.resolved);
            }
            Settle::Settled {
                balances, version, ..
            } => self
                .ledger
                .settle(version.0, &balances, &mut base, &mut self.resolved),
        }
    }

    /// Records how each transaction whose reservations have all resolved
    /// fared, crediting back the fulfilled reservations of each one
    /// cancelled, then notifies their keys. Credits may resolve more, so this
    /// repeats until none do.
    fn release(&mut self) {
        let mut released = Vec::new();
        while !self.resolved.is_empty() {
            for (ticket, status) in std::mem::take(&mut self.resolved) {
                let (id, index) = self.owners.remove(&ticket).expect("a ticket resolves once");
                let waiting = self.waiting.get_mut(&id).expect("its transaction waits");
                waiting.resolved[index] = Some(status);
                waiting.unresolved -= 1;
                if waiting.unresolved > 0 {
                    continue;
                }
                let Waiting {
                    withdrawal,
                    resolved,
                    ..
                } = self.waiting.remove(&id).expect("waiting");
                let status = if resolved.contains(&Some(ReservationStatus::Cancelled)) {
                    for (i, reservation) in withdrawal.reservations.iter().enumerate() {
                        if resolved[i] == Some(ReservationStatus::Fulfilled) {
                            self.ledger.credit(
                                reservation.account,
                                reservation.amount,
                                reservation.version.0,
                                &mut self.resolved,
                            );
                        }
                    }
                    ReservationStatus::Cancelled
                } else {
                    ReservationStatus::Fulfilled
                };
                released.push((
                    InputKey::Reserved(withdrawal.digest),
                    InputValue::Reserved(status),
                ));
            }
        }
        if !released.is_empty() {
            push(&self.inputs, Command::Resolve(released), "input waiter");
        }
    }
}

impl Processor for FundsReserver {
    type Input = FundsCommand;
    type Output = ();

    fn process(&mut self, command: FundsCommand, _bump: &mut Bump) -> Option<()> {
        match command {
            FundsCommand::Reserve(withdrawals) => {
                for withdrawal in withdrawals {
                    self.reserve(withdrawal);
                }
            }
            FundsCommand::Settle(settles) => {
                for settle in settles {
                    self.settle(settle);
                }
            }
        }
        self.release();
        None
    }
}
