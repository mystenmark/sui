// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution with anchovy's executor: the transaction's inputs loaded from the store as views,
//! executed in an arena, and the outputs handed back as the store's commit.

use std::sync::Arc;

use containers::Bump;
use exec_types::object::Object;
use executor::execution_engine::{ExecutionOutput, execute_transaction_to_effects};
use executor::execution_mode::Normal;
use executor::execution_params::ExecutionOrEarlyError;
use executor::gas::SuiGasStatus;
use executor::inputs::{ExecutionInputs, InputObjectKind, InputState, LoadedInput};
use executor::storage::EmptyUnsettledObjectFunds;
use messages::Kept;
use messages::base::TransactionDigest;
use messages::transaction::{HasDigest, TransactionData, TxState};
use move_vm_runtime::runtime::MoveRuntime;
use sui_protocol_config::ProtocolConfig;
use sui_types::metrics::ExecutionMetrics;
use validation::inputs::{InputKind, input_objects, receiving_objects};

use crate::reads::StoreReads;
use crate::{Error, Outcome, Result};

/// The arena's first chunk: most transactions fit in it.
const ARENA_CAPACITY: usize = 1 << 20;

/// Anchovy's executor for one epoch.
pub struct NativeExecution {
    /// The transactions' arena, reset after each: execution is serial, so the lock is
    /// uncontended.
    arena: std::sync::Mutex<Bump>,
    /// Built once from `config`: the natives' costs are fixed for the epoch.
    natives_cost_table: natives::NativesCostTable,
    config: ProtocolConfig,
    move_vm: Arc<MoveRuntime>,
    metrics: Arc<ExecutionMetrics>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    reference_gas_price: u64,
}

impl NativeExecution {
    pub fn new(
        config: ProtocolConfig,
        metrics: Arc<ExecutionMetrics>,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
    ) -> Result<NativeExecution> {
        let move_vm = executor::adapter::new_move_runtime(
            natives::all_natives(/* silent */ true, &config),
            &config,
        )
        .map_err(|e| Error::Native(e.0))?;
        Ok(NativeExecution {
            arena: std::sync::Mutex::new(Bump::with_capacity(ARENA_CAPACITY)),
            natives_cost_table: natives::NativesCostTable::from_protocol_config(&config),
            config,
            move_vm: Arc::new(move_vm),
            metrics,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
        })
    }

    /// Executes `transaction` (`SenderSignedData` BCS, whose inputs passed the input checks)
    /// against the store's live objects, as `Execution::execute` does with sui's executor.
    pub fn execute_bytes(&self, store: &store::Store, transaction: &[u8]) -> Result<Outcome> {
        let parsed = messages::Message::<
            messages::transaction::Transaction<'static, messages::transaction::DigestReady>,
        >::parse(transaction.to_vec())
        .map_err(|(e, _)| Error::Native(format!("{e:?}")))?;
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
        let mut arena = self.arena.lock().expect("the arena's lock is not poisoned");
        let outcome = self.execute_in(&arena, store, data, transaction);
        if arena.chunks() > 1 {
            // Grow the first chunk so that later transactions this large fit in it.
            *arena = Bump::with_capacity(arena.allocated().next_power_of_two().max(ARENA_CAPACITY));
        } else {
            arena.reset();
        }
        outcome
    }

    fn execute_in<S: TxState + HasDigest>(
        &self,
        bump: &Bump,
        store: &store::Store,
        data: &TransactionData<'_, S>,
        transaction: &[u8],
    ) -> Result<Outcome> {
        let kept = Kept::new();
        let reads = StoreReads::new(bump, store, &kept);
        let digest = *data.digest();

        let kinds = input_objects(data).map_err(|e| Error::Native(format!("{e:?}")))?;
        let mut loaded = containers::Vec::with_capacity_in(kinds.len(), bump);
        for kind in &kinds {
            loaded.push(load(&reads, kind)?);
        }
        let receiving = receiving_objects(data);
        let mut receiving_refs = containers::Vec::with_capacity_in(receiving.len(), bump);
        receiving_refs.extend(receiving.iter().map(|r| exec_types::base::object_ref(r)));
        // The root's live version is the version of its live object (the store keys objects by
        // version), without reading it.
        let accumulator_version = if self.config.enable_accumulators() {
            reads
                .live(&exec_types::base::SUI_ACCUMULATOR_ROOT_OBJECT_ID)?
                .map(|live| live.version)
        } else {
            None
        };
        let inputs = ExecutionInputs::new(bump, loaded, receiving_refs.leak(), accumulator_version);

        let gas_data = *data.gas_data();
        let gas_status = SuiGasStatus::new(
            gas_data.budget,
            gas_data.price,
            self.reference_gas_price,
            &self.config,
        )
        .map_err(|e| Error::Native(format!("{e:?}")))?;

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
            digest,
            &self.move_vm,
            &self.epoch,
            self.epoch_start_timestamp_ms,
            &self.config,
            self.metrics.clone(),
            false,
            ExecutionOrEarlyError::ok(None),
            Some(&self.natives_cost_table),
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
                    digest: digest_of(digest),
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
/// immutable objects at theirs, which must be the live one.
fn load<'a>(reads: &StoreReads<'a>, kind: &InputKind<'_>) -> Result<LoadedInput<'a>> {
    let (kind, object) = match kind {
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
            let object = reads
                .object_at_with_digest(&id, version, r.digest)?
                .ok_or(Error::MissingNative(id))?;
            (
                InputObjectKind::ImmOrOwnedMoveObject(exec_types::base::object_ref(r)),
                object,
            )
        }
        InputKind::Package(id) => (
            InputObjectKind::MovePackage(*id),
            reads.live_object(id)?.ok_or(Error::MissingNative(*id))?,
        ),
        InputKind::Shared(s) => (
            InputObjectKind::SharedMoveObject {
                id: s.id,
                initial_shared_version: s.initial_shared_version.get(),
                mutability: s.mutability(),
            },
            reads
                .live_object(&s.id)?
                .ok_or(Error::MissingNative(s.id))?,
        ),
    };
    Ok(LoadedInput::new(kind, InputState::Object(object)))
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

fn digest_of(digest: TransactionDigest) -> messages::base::Digest {
    digest
}
