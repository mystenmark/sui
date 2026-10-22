// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution with anchovy's executor: the transaction's inputs loaded from the store as views,
//! executed in an arena, and the outputs handed back as the store's commit.

use std::sync::Arc;

use containers::Bump;
use exec_types::object::Object;
use executor::data_store::cached_package_store::PackageCache;
use executor::execution_engine::{ExecutionOutput, execute_transaction_to_effects, is_system_tx};
use executor::execution_mode::Normal;
use executor::execution_params::{
    ExecutionOrEarlyError, FundsWithdrawStatus, get_early_execution_error,
};
use executor::gas::SuiGasStatus;
use executor::inputs::{ExecutionInputs, InputObjectKind, InputState, LoadedInput};
use executor::static_programmable_transactions::type_cache::TypeCache;
use executor::storage::EmptyUnsettledObjectFunds;
use messages::base::{ObjectId, TransactionDigest};
use messages::transaction::{DigestReady, HasDigest, Transaction, TransactionData, TxState};
use messages::{Kept, Message};
use move_vm_runtime::runtime::MoveRuntime;
use sui_protocol_config::ProtocolConfig;
use sui_types::metrics::ExecutionMetrics;
use validation::inputs::{InputKind, input_objects, receiving_objects};

use crate::reads::StoreReads;
use crate::{Error, Funds, Input, Outcome, Result};

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
    /// Execution is serial, so the lock is uncontended.
    state: std::sync::Mutex<State>,
    /// Built once from `config`: the natives' costs are fixed for the epoch.
    natives_cost_table: natives::NativesCostTable,
    /// Packages a runtime may resolve before it is replaced.
    max_packages: usize,
    config: ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    reference_gas_price: u64,
}

/// What execution keeps between transactions.
struct State {
    /// The transactions' arena, reset after each.
    arena: Bump,
    runtime: Runtime,
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
    ) -> Result<NativeExecution> {
        Ok(NativeExecution {
            state: std::sync::Mutex::new(State {
                arena: Bump::with_capacity(ARENA_CAPACITY),
                runtime: Runtime::new(&config)?,
            }),
            natives_cost_table: natives::NativesCostTable::from_protocol_config(&config),
            max_packages: MAX_PACKAGES,
            config,
            metrics,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
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
        self.execute_at(store, data, transaction, &[], Funds::MaybeSufficient, None)
    }

    /// `execute`, but with inputs, funds and the accumulator root's version as
    /// `Reference::execute_at` takes them. A system transaction runs
    /// unmetered.
    pub fn execute_at<S: TxState + HasDigest>(
        &self,
        store: &store::Store,
        data: &TransactionData<'_, S>,
        transaction: &[u8],
        provided: &[(ObjectId, Input<'_>)],
        funds: Funds,
        assigned_accumulator: Option<u64>,
    ) -> Result<Outcome> {
        let mut state = self.state.lock().expect("the state's lock is not poisoned");
        let State { arena, runtime } = &mut *state;
        let outcome = self.execute_in(
            arena,
            runtime,
            store,
            data,
            transaction,
            provided,
            funds,
            assigned_accumulator,
        );
        arena.reset();
        if runtime.package_cache.len() > self.max_packages {
            *runtime = Runtime::new(&self.config).expect("a runtime builds as it did at the start");
        }
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
        assigned_accumulator: Option<u64>,
    ) -> Result<Outcome> {
        let kept = Kept::new();
        let reads = StoreReads::new(bump, store, &kept);
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
                .map(|live| assigned_accumulator.unwrap_or(live.version))
        } else {
            None
        };
        let inputs = ExecutionInputs::new(bump, loaded, receiving_refs.leak(), accumulator_version);

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

        let ExecutionOutput {
            inner_store,
            effects,
            ..
        } = execute_transaction_to_effects::<Normal>(
            bump,
            &reads,
            &inputs,
            &EmptyUnsettledObjectFunds,
            gas_data,
            gas_status,
            *data.kind(),
            None,
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
