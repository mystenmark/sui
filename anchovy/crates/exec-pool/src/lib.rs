// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Executing transactions whose inputs are available, at the versions their
//! ordinals resolve to (`docs/consensus_pipeline.md`, Execution).

use std::sync::Arc;

use containers::Bump;
use execution::{Funds, Input};
use messages::Message;
use messages::effects::{
    AccumulatorOperation, AccumulatorValue, ObjectOut, TransactionEffects, VersionedEffects,
};
use messages::object::{Object, Owner};
use messages::transaction::SharedObjectMutability;
use validator::consensus::sequence::SharedAccess;
use validator::epoch::EpochState;
use validator::epoch_store::EpochStore;
use validator::pipeline::items::{
    EffectsWaitItem, EpochCloserInput, Executable, ExecutableTransaction, FundsCommand,
    InputCommand, InputValue, ReservationStatus, Settle, SettlementInput, SystemKind,
    SystemTransaction,
};
use validator::pipeline::keys::{AccumulatorVersion, EffectsKey, InputKey, StreamId, TxDigest};
use validator::pipeline::tables::{Generation, GenerationTable};
use waiter::{Availability, Command};
use workqueue::{Processor, PushError, Queue, Sink};

/// Executes ready transactions. Resolves each shared access to an exact
/// version from the generation table, executes, commits the outputs,
/// records each generation it produced, then notifies.
///
/// - **Resolving.** Generation `k > 0` of a round is the version its writer
///   produced. Generation 0 is the earlier round's generation the resolver
///   gave (`resolved_base`) or, for a stream it had none for, the version
///   the stream starts the epoch at (`Store::start_version`), recorded as
///   generation 0 of the round by the first transaction to read it. Invariant: the round's writer of generation 0
///   records it before it commits, so a reader finding no entry has seen no
///   write of the round. That holds with several workers too: the resolver
///   gives a stream `StreamLive` only in the round that first touches it,
///   and later rounds wait for that round's generation 0.
///
///   A stream the resolver had no base for is always live: voting accepts
///   only live shared inputs, and a deletion since the epoch began reaches
///   later rounds as a base, recorded `Ended`.
/// - **Recording.** Each stream it writes exclusively gets generation
///   `k + 1`: the version its effects give, or `Ended` if it deleted or
///   wrapped the object or took it out of consensus. A stream its effects do
///   not change (an input that had ended, say) keeps the generation it read.
/// - **Executed already** (a replayed commit): it is not executed again, but
///   its generations are recorded from its stored effects.
/// - **Execution never fails.** Every transaction released here can run
///   (it may abort, with effects); an error outside the transaction (the
///   store, an input not found) is a bug, and panics.
///
/// Every key is notified only after the store and the generation table hold
/// it, once per batch. One worker for now: the generation table's
/// `get_or_record` keeps generation 0 consistent if several run.
///
/// Each transaction runs with its chunk's accumulator root version
/// (`Executable::root_version`), as the reference assigns it. Not yet:
/// cancelled transactions do not exist yet.
///
/// Inward:
/// - input waiter, ready: `Vec<Executable>`.
///
/// Outward:
/// - input waiter, notify: `ObjectVersion` of each output, `Package` of each
///   package written, and `Generation(stream, round, k + 1)` of each
///   exclusive write (and `Generation(stream, round, 0)` of each it
///   resolved from the live object);
/// - effects waiter, notify: `EffectsKey` of each transaction executed;
/// - `FundsReserver`: `Settle::Deposit` of each balance deposit a user
///   transaction of commit `v` made, settling into `v + 1`; and
///   `Settle::Credit` of what it withdrew short of each reservation.
///
/// A transaction with funds reservations runs only if the reservation table
/// says all were fulfilled; otherwise it fails without running. Its entry
/// is removed once it has executed.
///
/// Reads and writes: the object store, the generation table and the
/// reservation table.
pub struct Executor {
    store: Arc<store::Store>,
    epoch: Arc<EpochState>,
    epoch_store: EpochStore,
    generations: Arc<GenerationTable>,
    inputs: Queue<InputCommand>,
    effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    funds: Queue<FundsCommand>,
    /// Scratch, per transaction: each shared access's generation.
    resolved: Vec<Generation>,
}

/// The objects a transaction wrote: id, version, object.
type WrittenObjects<'b> =
    containers::Vec<'b, (messages::base::ObjectId, u64, Message<Object<'static>>)>;

/// A written object's value: the object as the input waiter resolves it.
fn object(bytes: Vec<u8>) -> Message<Object<'static>> {
    Message::parse(bytes)
        .map_err(|(e, _)| e)
        .expect("a written object parses")
}

impl Executor {
    pub fn new(
        store: Arc<store::Store>,
        epoch: Arc<EpochState>,
        epoch_store: EpochStore,
        generations: Arc<GenerationTable>,
        inputs: Queue<InputCommand>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        funds: Queue<FundsCommand>,
    ) -> Executor {
        Executor {
            store,
            epoch,
            epoch_store,
            generations,
            inputs,
            effects,
            funds,
            resolved: Vec::new(),
        }
    }

    /// Executes `executable` from the inputs its keys resolved to, records
    /// what it produced, and pushes the keys it resolves.
    ///
    /// # Panics
    /// On a store that fails, or a key the input waiter released it on
    /// without a value.
    fn execute(
        &mut self,
        executable: &Executable,
        resolved_keys: &mut Vec<(InputKey, InputValue)>,
        effects_keys: &mut Vec<EffectsKey>,
        deposits: &mut Vec<Settle>,
        bump: &Bump,
    ) {
        let entry = &executable.entry;
        let (digest, bytes): (TxDigest, &[u8]) = match &entry.transaction {
            ExecutableTransaction::User(transaction) => {
                let signed = &transaction.get().0;
                (*signed.digest(), signed.bytes())
            }
            ExecutableTransaction::System(system) => (system.digest, &system.bytes),
        };

        let stored = self.stored_effects(digest);
        // Executed and committed before a restart: its checkpoint will not
        // commit again, so what it produced is written at once.
        let committed = stored.is_some() && !self.store.is_uncommitted(&digest);

        // 1. Its inputs, from the values its keys resolved to: the shared
        //    accesses' first, in access order (as the resolver keys them),
        //    then objects and packages, then its reservations.
        let mut values = executable
            .inputs
            .iter()
            .map(|v| v.as_ref().expect("every key resolved"));
        self.resolved.clear();
        let mut provided = containers::Vec::new_in(bump);
        for access in &entry.shared {
            let generation = match values.next().expect("a value per shared access") {
                InputValue::Generation(generation, object) => {
                    provided.push((
                        access.id,
                        match (generation, object) {
                            (Generation::Version { .. }, Some(object)) => {
                                Input::Object(object.get().bytes)
                            }
                            (
                                Generation::Ended {
                                    version,
                                    transaction,
                                },
                                _,
                            ) => Input::Ended {
                                version: *version,
                                transaction: *transaction,
                            },
                            (Generation::Version { .. }, None) => {
                                unreachable!("a version's object comes with it")
                            }
                        },
                    ));
                    *generation
                }
                InputValue::Live => {
                    let generation = self.live_generation(access, resolved_keys);
                    let Generation::Version { version, .. } = generation else {
                        unreachable!("generation 0 of a live stream is a version");
                    };
                    provided.push((access.id, Input::At(version)));
                    generation
                }
                other => panic!("a shared access resolved to {other:?}"),
            };
            self.resolved.push(generation);
        }
        let mut funds = Funds::MaybeSufficient;
        for value in values {
            match value {
                InputValue::Object(object) => provided.push((
                    *object.get().id().expect("an input object has an id"),
                    Input::Object(object.get().bytes),
                )),
                InputValue::Reserved(ReservationStatus::Fulfilled) => {}
                InputValue::Reserved(ReservationStatus::Cancelled) => funds = Funds::Insufficient,
                other => panic!("an object input resolved to {other:?}"),
            }
        }

        // 2. Its effects: stored, or from executing and committing it.
        let (effects, written) = match stored {
            Some(effects) => (effects, containers::Vec::new_in(bump)),
            None => self.execute_now(
                digest,
                bytes,
                &provided,
                funds,
                executable.root_version,
                bump,
            ),
        };
        drop(provided);
        let VersionedEffects::V2(effects_v2) = &effects.get().version else {
            unreachable!("execution produces V2 effects");
        };
        if let ExecutableTransaction::System(SystemTransaction {
            kind: SystemKind::Barrier { .. },
            ..
        }) = &entry.transaction
        {
            // `CommitStage` numbers each chunk's root version by counting
            // barriers: each must write exactly the next.
            assert_eq!(
                effects_v2.lamport_version,
                executable.root_version + 1,
                "a barrier writes the root's next version"
            );
        }

        // 3. The generations it produced, then its outputs, to resolve.
        let object_at = |id: &messages::base::ObjectId, version: u64| -> Message<Object<'static>> {
            match written.iter().find(|(w, v, _)| w == id && *v == version) {
                Some((_, _, object)) => object.clone(),
                None => self
                    .store
                    .object(id, version)
                    .expect("the store reads")
                    .expect("an executed transaction's outputs are stored"),
            }
        };
        for (i, access) in entry.shared.iter().enumerate() {
            if access.mutability != SharedObjectMutability::Mutable {
                continue;
            }
            let stream = stream_of(access);
            let produced = produced(effects_v2, stream, self.resolved[i], digest);
            let round = access.ordinal.round;
            let generation = access.ordinal.writes_before + 1;
            self.generations
                .record(stream, round, generation, produced, digest, committed);
            let object = match produced {
                Generation::Version { version, .. } => Some(object_at(&access.id, version)),
                Generation::Ended { .. } => None,
            };
            resolved_keys.push((
                InputKey::Generation {
                    stream,
                    round,
                    generation,
                },
                InputValue::Generation(produced, object),
            ));
        }
        if let ExecutableTransaction::User(_) = &entry.transaction {
            funds_settled(effects_v2, executable, funds, deposits);
        }
        for change in effects_v2.changed_objects {
            match change.output_state {
                ObjectOut::ObjectWrite(..) => {
                    let version = effects_v2.lamport_version;
                    resolved_keys.push((
                        InputKey::ObjectVersion {
                            id: *change.id,
                            version,
                        },
                        InputValue::Object(object_at(change.id, version)),
                    ));
                }
                ObjectOut::PackageWrite(version, _) => {
                    let package = object_at(change.id, version);
                    resolved_keys.push((
                        InputKey::ObjectVersion {
                            id: *change.id,
                            version,
                        },
                        InputValue::Object(package.clone()),
                    ));
                    resolved_keys
                        .push((InputKey::Package(*change.id), InputValue::Object(package)));
                }
                ObjectOut::NotExist | ObjectOut::AccumulatorWriteV1(_) => {}
            }
        }
        effects_keys.push(EffectsKey(digest.into()));
    }

    /// The effects of the transaction with `digest`, if it executed already.
    fn stored_effects(&self, digest: TxDigest) -> Option<Message<TransactionEffects<'static>>> {
        let effects_digest = self
            .store
            .executed_effects(&digest)
            .expect("the store reads")?;
        Some(
            self.store
                .effects(&effects_digest)
                .expect("the store reads")
                .expect("executed effects are stored with their transaction"),
        )
    }

    /// The effects of executing `bytes` with `provided` inputs, written to
    /// the store, and the objects it wrote.
    ///
    /// # Panics
    /// If execution fails outside the transaction: everything it reads was
    /// waited for, so that is a bug.
    fn execute_now<'b>(
        &self,
        digest: TxDigest,
        bytes: &[u8],
        provided: &[(messages::base::ObjectId, Input<'_>)],
        funds: Funds,
        root_version: u64,
        bump: &'b Bump,
    ) -> (Message<TransactionEffects<'static>>, WrittenObjects<'b>) {
        let outcome = self
            .epoch
            .execution
            .execute_at(&self.store, bytes, provided, funds, Some(root_version))
            .unwrap_or_else(|e| panic!("transaction {digest:?} failed to execute: {e:?}"));
        let mut written = containers::Vec::with_capacity_in(outcome.commit.written.len(), bump);
        written.extend(
            outcome
                .commit
                .written
                .iter()
                .map(|w| (w.id, w.version, object(w.bytes.clone()))),
        );
        self.store.write(outcome.commit);
        let effects = Message::parse(outcome.effects)
            .map_err(|(e, _)| e)
            .expect("execution's effects parse");
        (effects, written)
    }

    /// Generation 0 of `access`'s stream in its round, which the resolver
    /// gave no base: as recorded if a transaction of the round recorded it,
    /// else the version the stream starts the epoch at, recorded now and
    /// resolved.
    ///
    /// # Panics
    /// If the stream has no live object (see `Executor`).
    fn live_generation(
        &self,
        access: &SharedAccess,
        resolved_keys: &mut Vec<(InputKey, InputValue)>,
    ) -> Generation {
        let stream = stream_of(access);
        let round = access.ordinal.round;
        if let Some(generation) = self.generations.get(stream, round, 0) {
            return generation;
        }
        let generation = self.generations.get_or_record(stream, round, 0, || {
            let start = self
                .epoch_store
                .start_version(&self.store, &stream.id, stream.initial_shared_version)
                .unwrap_or_else(|| panic!("{stream:?}, which voting found live, is live"));
            Generation::Version {
                version: start.version,
                digest: start.digest.bytes,
            }
        });
        let Generation::Version { version, .. } = generation else {
            unreachable!("generation 0 of a live stream is a version");
        };
        let object = self
            .store
            .object(&stream.id, version)
            .expect("the store reads")
            .expect("a live object is stored");
        resolved_keys.push((
            InputKey::Generation {
                stream,
                round,
                generation: 0,
            },
            InputValue::Generation(generation, Some(object)),
        ));
        generation
    }
}

/// What `effects` of user transaction `executable` tell `FundsReserver`:
/// each balance deposit, and, if its reservations were fulfilled, what it
/// withdrew short of each (all of it, if it failed). `FundsReserver` itself
/// credits back a cancelled transaction's.
fn funds_settled(
    effects: &messages::effects::TransactionEffectsV2<'_>,
    executable: &Executable,
    funds: Funds,
    settles: &mut Vec<Settle>,
) {
    let settles_into = AccumulatorVersion(executable.version.0 + 1);
    for change in effects.changed_objects {
        if let ObjectOut::AccumulatorWriteV1(write) = &change.output_state
            && write.operation == AccumulatorOperation::Merge
            && let AccumulatorValue::Integer(amount) = write.value
        {
            settles.push(Settle::Deposit(*change.id, amount, settles_into));
        }
    }
    if funds == Funds::Insufficient {
        return;
    }
    for reservation in &executable.reserved {
        let withdrawn: u64 = effects
            .changed_objects
            .iter()
            .filter(|c| *c.id == reservation.account)
            .filter_map(|c| match &c.output_state {
                ObjectOut::AccumulatorWriteV1(write)
                    if write.operation == AccumulatorOperation::Split =>
                {
                    match write.value {
                        AccumulatorValue::Integer(amount) => Some(amount),
                        _ => None,
                    }
                }
                _ => None,
            })
            .sum();
        let unused = reservation
            .amount
            .checked_sub(withdrawn)
            .expect("a transaction withdraws at most what it reserved");
        if unused > 0 {
            settles.push(Settle::Credit(
                reservation.account,
                unused,
                reservation.version,
            ));
        }
    }
}

fn stream_of(access: &SharedAccess) -> StreamId {
    StreamId {
        id: access.id,
        initial_shared_version: access.initial_shared_version,
    }
}

/// The generation of `stream` a transaction with `effects` produced, having
/// read `read`: the version it wrote while the object stays in the stream;
/// `Ended` if it deleted, wrapped or moved it out; what it read if its
/// effects do not change it.
fn produced(
    effects: &messages::effects::TransactionEffectsV2<'_>,
    stream: StreamId,
    read: Generation,
    digest: TxDigest,
) -> Generation {
    let Some(change) = effects.changed_objects.iter().find(|c| *c.id == stream.id) else {
        return read;
    };
    let isv = stream.initial_shared_version;
    match change.output_state {
        ObjectOut::ObjectWrite(object_digest, owner)
            if matches!(owner,
                Owner::Shared { initial_shared_version } if initial_shared_version == isv)
                || matches!(owner,
                Owner::ConsensusAddressOwner { start_version, .. } if start_version == isv) =>
        {
            Generation::Version {
                version: effects.lamport_version,
                digest: object_digest.bytes,
            }
        }
        _ => Generation::Ended {
            version: effects.lamport_version,
            transaction: digest.bytes,
        },
    }
}

/// Pushes `item` to a queue inside the pipeline, where a full queue is a
/// sizing bug; a closed one means the pipeline is shutting down.
fn push<T>(queue: &Queue<T>, item: T, name: &str) {
    if let Err(PushError::Full(_)) = queue.try_push(item) {
        panic!("the {name} queue refused work");
    }
}

impl Processor for Executor {
    type Input = Vec<Executable>;
    type Output = ();

    fn process(&mut self, ready: Vec<Executable>, bump: &mut Bump) -> Option<()> {
        let mut resolved_keys = Vec::new();
        let mut effects_keys = Vec::with_capacity(ready.len());
        let mut deposits = Vec::new();
        for executable in &ready {
            self.execute(
                executable,
                &mut resolved_keys,
                &mut effects_keys,
                &mut deposits,
                bump,
            );
        }
        // Everything is committed and recorded: now the keys are visible.
        if !resolved_keys.is_empty() {
            push(
                &self.inputs,
                Command::Resolve(resolved_keys),
                "input waiter",
            );
        }
        if !effects_keys.is_empty() {
            push(
                &self.effects,
                Command::notify(effects_keys),
                "effects waiter",
            );
        }
        if !deposits.is_empty() {
            push(
                &self.funds,
                FundsCommand::Settle(deposits),
                "funds reserver",
            );
        }
        None
    }
}

/// The effects waiter's availability check: effects the store holds as
/// executed.
pub struct EffectsAvailability {
    pub store: Arc<store::Store>,
}

impl Availability<EffectsKey> for EffectsAvailability {
    fn check(&mut self, keys: &[EffectsKey], found: &mut Vec<Option<()>>) {
        found.clear();
        // A key may be a digest a client named, so a miss is not cached.
        found.extend(keys.iter().map(|EffectsKey(digest)| {
            self.store
                .executed_effects_uncached(digest)
                .expect("the store reads")
                .map(|_| ())
        }));
    }
}

/// Where the effects waiter's ready items go, by kind: a `wait_for_effects`
/// is woken, to read the effects from the store; a chunk's effects, or its
/// settlements', go to `SettlementScheduler`; the change-epoch
/// transaction's, to `EpochCloser`. Runs on the effects waiter's thread.
///
/// Inward: effects waiter, ready: `Vec<EffectsWaitItem>`.
/// Outward: replies; `SettlementScheduler`: `SettlementInput`;
/// `EpochCloser`: `EpochCloserInput::ChangeEpochExecuted`.
pub struct EffectsRouter {
    pub settlement: Queue<SettlementInput>,
    pub epoch_closer: Queue<EpochCloserInput>,
}

impl Sink<Vec<EffectsWaitItem>> for EffectsRouter {
    fn send(&mut self, ready: Vec<EffectsWaitItem>) {
        for item in ready {
            match item {
                // The handler may have given up; nothing to do then.
                EffectsWaitItem::Executed(reply) => {
                    let _ = reply.send(());
                }
                EffectsWaitItem::ChunkExecuted(chunk) => push(
                    &self.settlement,
                    SettlementInput::ChunkExecuted(chunk),
                    "settlement",
                ),
                EffectsWaitItem::SettlementExecuted { height, digests } => push(
                    &self.settlement,
                    SettlementInput::SettlementExecuted { height, digests },
                    "settlement",
                ),
                EffectsWaitItem::ChangeEpochExecuted => push(
                    &self.epoch_closer,
                    EpochCloserInput::ChangeEpochExecuted,
                    "epoch closer",
                ),
            }
        }
    }
}

/// The executed transaction's results, read back from the store.
#[cfg(test)]
mod tests {
    use messages::base::ObjectId;
    use messages::build::base::{
        AccountAddress, Digest, ObjectId as BuildId, SequenceNumber, SuiAddress, TransactionDigest,
    };
    use messages::build::effects::{
        AccumulatorAddress, AccumulatorOperation as Op, AccumulatorValue as Value,
        AccumulatorWriteV1, EffectsObjectChange, GasCostSummary, IdOperation, ObjectIn,
        ObjectOut as Out, TransactionEffects as Effects, TransactionEffectsV2 as EffectsV2,
    };
    use messages::build::execution_status::ExecutionStatus;
    use messages::build::type_tag::TypeTag;
    use validator::pipeline::items::{ScheduledEntry, SystemKind, SystemTransaction};
    use validator::pipeline::keys::Reservation;

    use super::*;

    fn write(account: u8, operation: Op, amount: u64) -> (BuildId, EffectsObjectChange) {
        (
            BuildId(AccountAddress([account; 32])),
            EffectsObjectChange {
                input_state: ObjectIn::NotExist,
                output_state: Out::AccumulatorWriteV1(AccumulatorWriteV1 {
                    address: AccumulatorAddress {
                        address: SuiAddress([0; 32]),
                        ty: TypeTag::U64,
                    },
                    operation,
                    value: Value::Integer(amount),
                }),
                id_operation: IdOperation::None,
            },
        )
    }

    fn effects(
        changed_objects: Vec<(BuildId, EffectsObjectChange)>,
    ) -> Message<TransactionEffects<'static>> {
        let bytes = bcs::to_bytes(&Effects::V2(Box::new(EffectsV2 {
            status: ExecutionStatus::Success,
            executed_epoch: 0,
            gas_used: GasCostSummary {
                computation_cost: 0,
                storage_cost: 0,
                storage_rebate: 0,
                non_refundable_storage_fee: 0,
            },
            transaction_digest: TransactionDigest(Digest([1; 32])),
            gas_object_index: None,
            events_digest: None,
            dependencies: vec![],
            lamport_version: SequenceNumber(2),
            changed_objects,
            unchanged_consensus_objects: vec![],
            aux_data_digest: None,
        })))
        .unwrap();
        Message::parse(bytes).map_err(|(e, _)| e).unwrap()
    }

    fn executable(reserved: &[(u8, u64)]) -> Executable {
        let version = AccumulatorVersion(7);
        Executable {
            version,
            root_version: 1,
            reserved: reserved
                .iter()
                .map(|&(account, amount)| Reservation {
                    account: ObjectId([account; 32]),
                    amount,
                    version,
                })
                .collect(),
            height: 0,
            entry: ScheduledEntry {
                transaction: ExecutableTransaction::System(SystemTransaction {
                    kind: SystemKind::Barrier { height: 0 },
                    bytes: vec![],
                    digest: messages::base::Hashed::of("TransactionData", &[]),
                }),
                shared: vec![],
                origin: None,
            },
            resolved_base: vec![],
            inputs: vec![],
        }
    }

    fn settled(
        effects: &Message<TransactionEffects<'static>>,
        reserved: &[(u8, u64)],
    ) -> Vec<Settle> {
        let VersionedEffects::V2(v2) = &effects.get().version else {
            unreachable!()
        };
        let mut settles = Vec::new();
        funds_settled(
            v2,
            &executable(reserved),
            Funds::MaybeSufficient,
            &mut settles,
        );
        settles
    }

    #[test]
    fn a_cancelled_transaction_credits_nothing_itself() {
        let e = effects(vec![write(2, Op::Merge, 9)]);
        let VersionedEffects::V2(v2) = &e.get().version else {
            unreachable!()
        };
        let mut settles = Vec::new();
        funds_settled(
            v2,
            &executable(&[(1, 100)]),
            Funds::Insufficient,
            &mut settles,
        );
        assert_eq!(
            settles,
            [Settle::Deposit(ObjectId([2; 32]), 9, AccumulatorVersion(8))]
        );
    }

    #[test]
    fn deposits_settle_into_the_next_version_and_shortfalls_are_credited() {
        let e = effects(vec![write(1, Op::Split, 30), write(2, Op::Merge, 9)]);
        assert_eq!(
            settled(&e, &[(1, 100), (3, 5)]),
            [
                Settle::Deposit(ObjectId([2; 32]), 9, AccumulatorVersion(8)),
                Settle::Credit(ObjectId([1; 32]), 70, AccumulatorVersion(7)),
                // Nothing withdrawn from 3.
                Settle::Credit(ObjectId([3; 32]), 5, AccumulatorVersion(7)),
            ]
        );
    }

    #[test]
    fn a_reservation_withdrawn_in_full_credits_nothing() {
        let e = effects(vec![write(1, Op::Split, 100)]);
        assert_eq!(settled(&e, &[(1, 100)]), []);
    }

    #[test]
    fn a_net_deposit_to_a_reserved_account_credits_all_of_it() {
        // Effects net each account's writes: withdrawing 10 and depositing 25
        // shows as a deposit of 15.
        let e = effects(vec![write(1, Op::Merge, 15)]);
        assert_eq!(
            settled(&e, &[(1, 10)]),
            [
                Settle::Deposit(ObjectId([1; 32]), 15, AccumulatorVersion(8)),
                Settle::Credit(ObjectId([1; 32]), 10, AccumulatorVersion(7)),
            ]
        );
    }
}
