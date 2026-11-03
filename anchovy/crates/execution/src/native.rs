// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution with anchovy's executor: the transaction's inputs loaded from the store as views,
//! executed in an arena, and the outputs handed back as the store's commit.

use std::sync::{Arc, PoisonError};

use containers::Bump;
use exec_types::object::Object;
use exec_types::storage::RuntimeObjectResolver as _;
use executor::data_store::cached_package_store::PackageCache;
use executor::execution_engine::{ExecutionOutput, execute_transaction_to_effects, is_system_tx};
use executor::execution_mode::Normal;
use executor::execution_params::{
    ExecutionOrEarlyError, FundsWithdrawStatus, get_early_execution_error,
};
use executor::gas::SuiGasStatus;
use executor::inputs::{ExecutionInputs, InputObjectKind, InputState, LoadedInput};
use executor::static_programmable_transactions::type_cache::TypeCache;
use executor::storage::UnsettledObjectFundsRead;
use messages::arena::Ref;
use messages::base::{ObjectId, SuiAddress, TransactionDigest};
use messages::object::MoveObjectType;
use messages::transaction::{
    CallArg, DigestReady, FundsWithdrawalArg, HasDigest, ObjectArg, ProgrammableTransaction,
    Reservation, Transaction, TransactionData, TransactionKind, TxState, WithdrawFrom,
    WithdrawalTypeArg,
};
use messages::{Kept, Message};
use move_vm_runtime::runtime::MoveRuntime;
use sui_protocol_config::ProtocolConfig;
use sui_types::metrics::ExecutionMetrics;
use validation::inputs::{InputKind, input_objects, receiving_objects};

use crate::reads::StoreReads;
use crate::{Awaited, Error, Funds, Input, Outcome, Result, Unsettled};

/// The arena's first chunk: most transactions fit in it.
const ARENA_CAPACITY: usize = 1 << 20;

/// `transaction`, `SenderSignedData` BCS, parsed.
pub fn parse(transaction: &[u8]) -> Result<Message<Transaction<'static, DigestReady>>> {
    Message::parse(transaction.to_vec()).map_err(|(e, _)| Error::Native(format!("{e:?}")))
}

/// Packages a runtime may resolve before it is replaced, by default: it keeps every package it
/// resolves, so this bounds its memory however many packages an epoch's transactions use.
const MAX_PACKAGES: usize = 1024;

/// Anchovy's executor for one epoch.
pub struct NativeExecution {
    /// States no execution holds: each concurrent execution takes one, or
    /// builds one if none is free, and puts it back.
    idle: std::sync::Mutex<Vec<State>>,
    /// Built once from `config`: the natives' costs are fixed for the epoch.
    natives_cost_table: natives::NativesCostTable,
    /// Packages a runtime may resolve before it is replaced.
    max_packages: usize,
    config: ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    reference_gas_price: u64,
    /// Coin reservations' ids are masked with it.
    chain_identifier: [u8; 32],
}

/// What an execution keeps between transactions.
struct State {
    /// The transactions' arena, reset after each.
    arena: Bump,
    runtime: Runtime,
}

impl State {
    fn new(config: &ProtocolConfig) -> Result<State> {
        Ok(State {
            arena: Bump::with_capacity(ARENA_CAPACITY),
            runtime: Runtime::new(config)?,
        })
    }
}

/// A Move runtime, and the caches filled from it: replaced together.
struct Runtime {
    move_vm: Arc<MoveRuntime>,
    /// The packages `move_vm` resolved, filled from it only.
    package_cache: Arc<PackageCache>,
    /// The types and layouts the VM computed. It must not outlive the epoch, at whose end the
    /// system packages change in place.
    type_cache: Arc<TypeCache>,
}

impl Runtime {
    fn new(config: &ProtocolConfig) -> Result<Runtime> {
        let move_vm = executor::adapter::new_move_runtime(
            natives::all_natives(/* silent */ true, config),
            config,
        )
        .map_err(|e| Error::Native(e.0))?;
        Ok(Runtime {
            move_vm: Arc::new(move_vm),
            package_cache: Arc::new(PackageCache::new()),
            type_cache: Arc::new(TypeCache::new()),
        })
    }
}

impl NativeExecution {
    pub fn new(
        config: ProtocolConfig,
        metrics: Arc<ExecutionMetrics>,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
        chain_identifier: [u8; 32],
    ) -> Result<NativeExecution> {
        Ok(NativeExecution {
            idle: std::sync::Mutex::new(vec![State::new(&config)?]),
            natives_cost_table: natives::NativesCostTable::from_protocol_config(&config),
            max_packages: MAX_PACKAGES,
            config,
            metrics,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
            chain_identifier,
        })
    }

    /// Replaces the runtime once it has resolved more than `max` packages.
    #[must_use]
    pub fn with_max_packages(mut self, max: usize) -> NativeExecution {
        self.max_packages = max;
        self
    }

    /// Executes `transaction` (`SenderSignedData` BCS, whose inputs passed the input checks)
    /// against the store's live objects, as `Execution::execute` does with sui's executor.
    pub fn execute_bytes(&self, store: &store::Store, transaction: &[u8]) -> Result<Outcome> {
        let parsed = parse(transaction)?;
        self.execute(store, parsed.get().0.data(), transaction)
    }

    /// Executes `data` (whose inputs passed the input checks) against the store's live objects.
    /// Nothing is written: the outcome carries the commit.
    ///
    /// # Panics
    /// If an owned input is no longer live at the version and digest the transaction gives:
    /// equivocation, which consensus rules out.
    pub fn execute<S: TxState + HasDigest>(
        &self,
        store: &store::Store,
        data: &TransactionData<'_, S>,
        transaction: &[u8],
    ) -> Result<Outcome> {
        self.execute_at(
            store,
            data,
            transaction,
            &[],
            Funds::MaybeSufficient,
            crate::Implicit::default(),
            &crate::NoneUnsettled,
            &crate::NoneAwaited,
        )
    }

    /// `execute`, but with inputs, funds and implicit versions as
    /// `Reference::execute_at` takes them. A system transaction runs
    /// unmetered.
    pub fn execute_at<S: TxState + HasDigest>(
        &self,
        store: &store::Store,
        data: &TransactionData<'_, S>,
        transaction: &[u8],
        provided: &[(ObjectId, Input<'_>)],
        funds: Funds,
        implicit: crate::Implicit,
        unsettled: &dyn Unsettled,
        awaited: &dyn Awaited,
    ) -> Result<Outcome> {
        let idle = || self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = idle().pop();
        let mut state = match taken {
            Some(state) => state,
            None => State::new(&self.config)?,
        };
        let State { arena, runtime } = &mut state;
        let outcome = self.execute_in(
            arena,
            runtime,
            store,
            data,
            transaction,
            provided,
            funds,
            implicit,
            unsettled,
            awaited,
        );
        arena.reset();
        if runtime.package_cache.len() > self.max_packages {
            *runtime = Runtime::new(&self.config).expect("a runtime builds as it did at the start");
        }
        idle().push(state);
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_in<S: TxState + HasDigest>(
        &self,
        bump: &Bump,
        runtime: &Runtime,
        store: &store::Store,
        data: &TransactionData<'_, S>,
        transaction: &[u8],
        provided: &[(ObjectId, Input<'_>)],
        funds: Funds,
        implicit: crate::Implicit,
        unsettled: &dyn Unsettled,
        awaited: &dyn Awaited,
    ) -> Result<Outcome> {
        let kept = Kept::new();
        let reads = StoreReads::new(bump, store, &kept).awaiting(awaited);
        let digest = *data.digest();

        let kinds = input_objects(data, bump).map_err(|e| Error::Native(format!("{e:?}")))?;
        let mut loaded = containers::Vec::with_capacity_in(kinds.len(), bump);
        for kind in &kinds {
            loaded.push(load(&reads, kind, provided)?);
        }
        let receiving = receiving_objects(data, bump);
        let mut receiving_refs = containers::Vec::with_capacity_in(receiving.len(), bump);
        receiving_refs.extend(receiving.iter().map(|r| exec_types::base::object_ref(r)));
        // The root's live version is the version of its live object (the store keys objects by
        // version), without reading it.
        let accumulator_version = if self.config.enable_accumulators() {
            reads
                .live(&exec_types::base::SUI_ACCUMULATOR_ROOT_OBJECT_ID)?
                .map(|live| implicit.accumulator.unwrap_or(live.version))
        } else {
            None
        };
        let inputs = ExecutionInputs::new(
            bump,
            loaded,
            receiving_refs.leak(),
            accumulator_version,
            implicit.clock,
        );

        let gas_data = *data.gas_data();
        let gas_status = if is_system_tx(data.kind()) {
            SuiGasStatus::new_unmetered()
        } else {
            SuiGasStatus::new(
                gas_data.budget,
                gas_data.price,
                self.reference_gas_price,
                &self.config,
            )
            .map_err(|e| Error::Native(format!("{e:?}")))?
        };
        // An input whose stream ended, or a withdrawal not reserved, fails
        // the transaction before it runs.
        let funds = match funds {
            Funds::MaybeSufficient => FundsWithdrawStatus::MaybeSufficient,
            Funds::Insufficient => FundsWithdrawStatus::Insufficient,
        };
        let early = match get_early_execution_error(bump, &digest, &inputs, |_| false, &funds) {
            None => ExecutionOrEarlyError::ok(None),
            Some(errors) => ExecutionOrEarlyError::failed(errors, None),
        };
        // Skipped on an early error, as the reference's: the transaction
        // fails anyway, and a reservation's field may be gone.
        let (kind, rewritten) = if early.is_ok() {
            rewrite_coin_reservations(
                bump,
                &reads,
                *data.kind(),
                data.sender(),
                &self.chain_identifier,
                implicit.accumulator,
            )
        } else {
            (*data.kind(), None)
        };

        let unsettled = NativeUnsettled(unsettled);
        let ExecutionOutput {
            inner_store,
            effects,
            ..
        } = execute_transaction_to_effects::<Normal>(
            bump,
            &reads,
            &inputs,
            &unsettled,
            gas_data,
            gas_status,
            kind,
            rewritten,
            *data.sender(),
            *digest,
            &runtime.move_vm,
            &self.epoch,
            self.epoch_start_timestamp_ms,
            &self.config,
            self.metrics.clone(),
            false,
            early,
            Some(&self.natives_cost_table),
            Some(&runtime.type_cache),
            Some(&runtime.package_cache),
        );

        let written = inner_store.written.values().map(written).collect();
        let effects_bytes = effects.bytes.to_vec();
        Ok(Outcome {
            effects: effects_bytes.clone(),
            commit: store::Commit {
                written,
                removed: inner_store
                    .removed
                    .iter()
                    .map(|&(id, wrapped)| store::Removed {
                        id,
                        version: inner_store.lamport_version,
                        removal: if wrapped {
                            store::Removal::Wrapped
                        } else {
                            store::Removal::Deleted
                        },
                    })
                    .collect(),
                executed: Some(store::Executed {
                    digest,
                    transaction: transaction.to_vec(),
                    effects_digest: effects.digest,
                    effects: effects_bytes,
                    events: inner_store.encoded_events.map(|built| built.bytes.to_vec()),
                }),
            },
        })
    }
}

/// An input as execution reads it: packages and shared objects at their live version, owned and
/// immutable objects at theirs, which must be the live one; but one named in `provided` as given
/// there.
fn load<'a>(
    reads: &StoreReads<'a>,
    kind: &InputKind<'_>,
    provided: &[(ObjectId, Input<'_>)],
) -> Result<LoadedInput<'a>> {
    let (id, object_kind) = match kind {
        InputKind::ImmOrOwned(r) => (
            r.id,
            InputObjectKind::ImmOrOwnedMoveObject(exec_types::base::object_ref(r)),
        ),
        InputKind::Package(id) => (*id, InputObjectKind::MovePackage(*id)),
        InputKind::Shared(s) => (
            s.id,
            InputObjectKind::SharedMoveObject {
                id: s.id,
                initial_shared_version: s.initial_shared_version.get(),
                mutability: s.mutability(),
            },
        ),
    };
    let given = provided
        .iter()
        .find(|(p, _)| *p == id)
        .map(|(_, input)| *input);
    let state = match given {
        None => InputState::Object(live(reads, kind)?),
        Some(Input::Object(bytes)) => InputState::Object(reads.parse(bytes)?),
        Some(Input::At(version)) => InputState::Object(
            reads
                .object_at(&id, version)?
                .ok_or(Error::MissingNative(id))?,
        ),
        Some(Input::Ended {
            version,
            transaction,
        }) => InputState::ConsensusStreamEnded(version, TransactionDigest::new(transaction)),
        Some(Input::Cancelled(marker)) => InputState::Cancelled(marker),
    };
    Ok(LoadedInput::new(object_kind, state))
}

/// An input at its live version, which an owned or immutable input's must be.
fn live<'a>(
    reads: &StoreReads<'a>,
    kind: &InputKind<'_>,
) -> Result<exec_types::object::Object<'a>> {
    match kind {
        InputKind::ImmOrOwned(r) => {
            let id = r.id;
            let version = r.version.get();
            let live = reads.live(&id)?;
            assert!(
                live.is_some_and(|l| l.version == version && l.digest == r.digest),
                "equivocation: {id} at {version} {:?} is not live ({live:?})",
                r.digest
            );
            // Live at this version, so its digest is the live one just checked.
            reads
                .object_at_with_digest(&id, version, r.digest)?
                .ok_or(Error::MissingNative(id))
        }
        InputKind::Package(id) => reads.live_object(id)?.ok_or(Error::MissingNative(*id)),
        InputKind::Shared(s) => reads.live_object(&s.id)?.ok_or(Error::MissingNative(s.id)),
    }
}

/// The store's form of a written object, which the temporary store sealed.
fn written(object: &Object<'_>) -> store::Written {
    store::Written {
        id: object.id(),
        version: object.version(),
        digest: object.digest(),
        bytes: object
            .stored_bytes()
            .expect("written objects are sealed")
            .to_vec(),
    }
}

/// The reference's `rewrite_transaction_for_coin_reservations`: each coin
/// reservation input of a programmable transaction becomes a withdrawal of
/// its amount from the sender's balance of the type its accumulator field
/// holds, the field read as of the root's `version` (else its latest). Also
/// which inputs were rewritten, if any was.
///
/// # Panics
/// If a reservation does not name a balance field of the sender's: the
/// reference expects a certified transaction's reservations to resolve.
fn rewrite_coin_reservations<'a>(
    bump: &'a Bump,
    reads: &StoreReads<'a>,
    kind: TransactionKind<'a>,
    sender: &SuiAddress,
    chain_identifier: &[u8; 32],
    version: Option<u64>,
) -> (TransactionKind<'a>, Option<&'a [bool]>) {
    let reservation = |input: &CallArg<'a>| match *input {
        CallArg::Object(ObjectArg::ImmOrOwnedObject(r)) if r.is_coin_reservation() => Some(r),
        _ => None,
    };
    let TransactionKind::ProgrammableTransaction(pt) = kind else {
        return (kind, None);
    };
    if !pt.inputs.iter().any(|input| reservation(input).is_some()) {
        return (kind, None);
    }
    let mut inputs = containers::Vec::with_capacity_in(pt.inputs.len(), bump);
    let mut rewritten = containers::Vec::with_capacity_in(pt.inputs.len(), bump);
    for input in pt.inputs {
        let Some(r) = reservation(input) else {
            inputs.push(*input);
            rewritten.push(false);
            continue;
        };
        let field = ObjectId(std::array::from_fn(|i| r.id.0[i] ^ chain_identifier[i]));
        let object = reads
            .read_child_object(
                &exec_types::base::SUI_ACCUMULATOR_ROOT_OBJECT_ID,
                &field,
                version.unwrap_or(u64::MAX),
            )
            .expect("the store reads")
            .expect("a certified coin reservation names a field");
        let field = object.try_as_move().expect("a field is a Move object");
        let balance = match field.type_ {
            MoveObjectType::SuiBalanceAccumulatorField => {
                executor::accumulator_root::sui_type(bump)
            }
            MoveObjectType::BalanceAccumulatorField(balance) => balance,
            _ => panic!("a certified coin reservation names a balance field"),
        };
        // `Field<AccumulatorKey, U128>`: the field's UID, then the key's owner.
        assert!(
            field.contents[32..64] == sender.0,
            "a certified coin reservation is the sender's"
        );
        let amount = u64::from_le_bytes(r.digest.bytes[..8].try_into().expect("eight bytes"));
        let withdrawal = containers::alloc(
            bump,
            FundsWithdrawalArg {
                reservation: Reservation::MaxAmountU64(amount),
                type_arg: WithdrawalTypeArg::Balance(balance),
                withdraw_from: WithdrawFrom::Sender,
            },
        );
        inputs.push(CallArg::FundsWithdrawal(Ref::new(withdrawal)));
        rewritten.push(true);
    }
    let pt = ProgrammableTransaction {
        inputs: inputs.leak(),
        commands: pt.commands,
    };
    (
        TransactionKind::ProgrammableTransaction(pt),
        Some(rewritten.leak()),
    )
}

/// `Unsettled`, as anchovy's executor reads it.
struct NativeUnsettled<'u>(&'u dyn Unsettled);

impl UnsettledObjectFundsRead for NativeUnsettled<'_> {
    fn get_unsettled_object_withdraw(&self, account: &ObjectId, accumulator_version: u64) -> u128 {
        self.0.withdrawn(account, accumulator_version)
    }
}
