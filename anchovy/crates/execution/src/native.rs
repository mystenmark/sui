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
use messages::base::{ObjectId, TransactionDigest};
use messages::effects::TransactionEffects;
use messages::transaction::{HasDigest, TransactionData, TxState};
use move_vm_runtime::runtime::MoveRuntime;
use sui_protocol_config::ProtocolConfig;
use sui_types::metrics::ExecutionMetrics;
use validation::inputs::{InputKind, input_objects, receiving_objects};

use crate::reads::StoreReads;
use crate::{Error, Outcome, Result};

/// Anchovy's executor for one epoch.
pub struct NativeExecution {
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
        let bump = Bump::with_capacity(1 << 20);
        let kept = Kept::new();
        let reads = StoreReads::new(store, &kept);
        let digest = *data.digest();

        let kinds = input_objects(data).map_err(|e| Error::Native(format!("{e:?}")))?;
        let mut loaded = containers::Vec::with_capacity_in(kinds.len(), &bump);
        for kind in &kinds {
            loaded.push(load(&reads, kind)?);
        }
        let receiving = receiving_objects(data);
        let mut receiving_refs = containers::Vec::with_capacity_in(receiving.len(), &bump);
        receiving_refs.extend(receiving.iter().map(|r| exec_types::base::object_ref(r)));
        let accumulator_version = if self.config.enable_accumulators() {
            reads
                .live_object(&exec_types::base::SUI_ACCUMULATOR_ROOT_OBJECT_ID)?
                .map(|root| root.version())
        } else {
            None
        };
        let inputs =
            ExecutionInputs::new(&bump, loaded, receiving_refs.leak(), accumulator_version);

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
            &bump,
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
        );

        let written = inner_store.written.values().map(|o| written(o)).collect();
        let effects_bytes = effects.bytes.to_vec();
        let removed = removed(&effects_bytes)?;
        let events = if inner_store.events.is_empty() {
            None
        } else {
            Some(
                executor::effects::build_events(&bump, &inner_store.events)
                    .bytes
                    .to_vec(),
            )
        };
        Ok(Outcome {
            effects: effects_bytes.clone(),
            commit: store::Commit {
                written,
                removed,
                executed: Some(store::Executed {
                    digest: digest_of(digest),
                    transaction: transaction.to_vec(),
                    effects_digest: effects.digest,
                    effects: effects_bytes,
                    events,
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
            let object = reads
                .object_at(&id, version)?
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

/// Objects the transaction leaves without a live version.
fn removed(effects: &[u8]) -> Result<std::vec::Vec<ObjectId>> {
    let effects = messages::Message::<TransactionEffects<'static>>::parse(effects.to_vec())
        .map_err(|(e, _)| Error::Native(format!("{e:?}")))?;
    let messages::effects::VersionedEffects::V2(effects) = effects.get().version else {
        return Err(Error::Native("effects are V2".to_string()));
    };
    // Deleted, wrapped, and unwrapped then deleted: every change without an output.
    Ok(effects
        .changed_objects
        .iter()
        .filter(|c| matches!(c.output_state, messages::effects::ObjectOut::NotExist))
        .map(|c| *c.id)
        .collect())
}

fn digest_of(digest: TransactionDigest) -> messages::base::Digest {
    digest
}
