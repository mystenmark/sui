// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::cast_possible_truncation)]

use crate::{
    data_store::{
        cached_package_store::CachedPackageStore,
        transaction_package_store::TransactionPackageStore,
    },
    error::{ExecutionError, ExecutionErrorKind},
    execution::ResultWithTimings,
    execution_mode::ExecutionMode,
    execution_value::ExecutionState,
    gas_charger::GasCharger,
    static_programmable_transactions::{
        env::Env, linkage::analysis::LinkageAnalyzer, metering::translation_meter,
    },
};
use containers::{Bump, Vec};
use exec_types::storage::BackingPackageStore;
use exec_types::tx_context::TxContext;
use move_vm_runtime::runtime::MoveRuntime;
use natives::NativesCostTable;
use std::{cell::RefCell, rc::Rc, sync::Arc};
use sui_protocol_config::ProtocolConfig;
use sui_types::metrics::ExecutionMetrics;

pub mod env;
pub mod execution;
pub mod linkage;
pub mod loading;
pub mod metering;
pub mod spanned;
pub mod typing;

/// Options for [`execute_with_options`]. [`execute`] uses the `Default`.
#[derive(Clone, Copy)]
pub struct ExecuteOptions<'t> {
    /// Whether to save the wrapped object containers and generated object ids into the state
    /// view. Only the ownership invariant check reads them, and it runs only with
    /// `enable_expensive_checks`.
    pub record_invariant_bookkeeping: bool,
    /// The epoch's natives cost table, which must have been built from this `protocol_config`
    /// by `NativesCostTable::from_protocol_config`. `None` builds it for this transaction.
    pub natives_cost_table: Option<&'t NativesCostTable>,
}

impl Default for ExecuteOptions<'_> {
    fn default() -> Self {
        Self {
            record_invariant_bookkeeping: true,
            natives_cost_table: None,
        }
    }
}

/// Move tracing is not ported, so the reference's `trace_builder_opt` is left out.
pub fn execute<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    vm: &MoveRuntime,
    state_view: &mut dyn ExecutionState<'a>,
    package_store: &'a dyn BackingPackageStore<'a>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger<'a>,
    withdrawal_compatibility_inputs: Option<&[bool]>,
    txn: messages::transaction::ProgrammableTransaction<'a>,
) -> ResultWithTimings<'a, (), ExecutionError<'a>> {
    execute_with_options::<Mode>(
        bump,
        protocol_config,
        metrics,
        vm,
        state_view,
        package_store,
        tx_context,
        gas_charger,
        withdrawal_compatibility_inputs,
        txn,
        ExecuteOptions::default(),
    )
}

/// [`execute`], with `options`.
pub fn execute_with_options<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    vm: &MoveRuntime,
    state_view: &mut dyn ExecutionState<'a>,
    // Borrowed for the arena's lifetime: the transaction's package store keeps it.
    package_store: &'a dyn BackingPackageStore<'a>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger<'a>,
    // which inputs are withdrawals that need to be converted to coins
    withdrawal_compatibility_inputs: Option<&[bool]>,
    txn: messages::transaction::ProgrammableTransaction<'a>,
    options: ExecuteOptions<'_>,
) -> ResultWithTimings<'a, (), ExecutionError<'a>> {
    let gas_payment = gas_charger.gas_payment_amount();
    let package_store =
        CachedPackageStore::new(vm, TransactionPackageStore::new(bump, package_store));
    let linkage_analysis =
        LinkageAnalyzer::new::<Mode>(bump, protocol_config).map_err(|e| (e, Vec::new_in(bump)))?;
    let ptb_type_linkage = linkage_analysis
        .compute_input_type_resolution_linkage(&txn, &package_store, state_view)
        .and_then(|linkage| linkage.linkage_context())
        .map_err(|e| (e, Vec::new_in(bump)))?;
    let resolution_vm = vm
        .make_vm(&package_store.package_store, ptb_type_linkage)
        .map_err(|e| {
            (
                ExecutionError::new_with_source(ExecutionErrorKind::InvalidLinkage, e),
                Vec::new_in(bump),
            )
        })?;

    let mut env: Env<Mode> = Env::new(
        bump,
        protocol_config,
        vm,
        state_view,
        &package_store,
        &linkage_analysis,
        &resolution_vm,
    );
    let mut translation_meter =
        translation_meter::TranslationMeter::new(protocol_config, gas_charger);

    let txn = {
        let tx_context_ref = tx_context.borrow();
        loading::translate::transaction::<Mode>(
            &mut translation_meter,
            &env,
            &tx_context_ref,
            withdrawal_compatibility_inputs,
            gas_payment,
            txn,
        )
        .map_err(|e| (e, Vec::new_in(bump)))?
    };
    let txn = typing::translate_and_verify::<Mode>(&mut translation_meter, &env, txn)
        .map_err(|e| (e, Vec::new_in(bump)))?;
    execution::interpreter::execute::<Mode>(
        &mut env,
        metrics,
        tx_context,
        gas_charger,
        txn,
        options,
    )
}
