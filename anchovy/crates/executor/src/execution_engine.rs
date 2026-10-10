// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's `execution_engine`, from gas model 15 on (its `bump_only` path; the legacy
//! path is left out). Execution takes the transaction and its loaded inputs
//! (`ExecutionInputs`); effects come out as `messages::fast` bytes.

use crate::error::{ExecutionError, ExecutionErrorKind};
use crate::execution::{ExecutionTiming, ResultWithTimings};
use crate::execution_mode::{self, ExecutionMode};
use crate::execution_params::ExecutionOrEarlyError;
use crate::gas::{SuiGasStatus, SuiGasStatusAPI};
use crate::gas_charger::{GasCharger, PaymentKind, PaymentMethod};
use crate::inputs::ExecutionInputs;
use crate::ptb_builder::{CLOCK_MUT, ProgrammableTransactionBuilder};
use crate::static_programmable_transactions as SPT;
use crate::storage::UnsettledObjectFundsRead;
use crate::temporary_store::{InnerTemporaryStore, TemporaryStore};
use crate::transaction::is_gasless_transaction;
use containers::{Bump, Vec};
use exec_types::assert_invariant;
use exec_types::base::{EpochId, SUI_RANDOMNESS_STATE_OBJECT_ID};
use exec_types::object::Object;
use exec_types::storage::BackingStore;
use exec_types::tx_context::TxContext;
use messages::base::{Digest, ObjectId, SuiAddress, TransactionDigest};
use messages::effects::GasCostSummary;
use messages::execution_status::ExecutionStatus;
use messages::fast::Built;
use messages::transaction::{CallArg, GasData, ObjectArg, SharedObjectMutability, TransactionKind};
use move_vm_runtime::runtime::MoveRuntime;
use std::{cell::RefCell, rc::Rc, sync::Arc};
use sui_protocol_config::{LimitThresholdCrossed, ProtocolConfig, check_limit_by_meter};
use sui_types::metrics::ExecutionMetrics;

/// Whether `InsufficientFundsForWithdraw` appears anywhere in the early-error list.
fn any_error_is_insufficient_funds_for_withdraw(
    execution_params: &ExecutionOrEarlyError<'_>,
) -> bool {
    execution_params.early_errors().is_some_and(|errors| {
        errors
            .iter()
            .any(|e| matches!(e, ExecutionErrorKind::InsufficientFundsForWithdraw))
    })
}

/// Whether to short-circuit an IFFW transaction. Matches the legacy short-circuit once
/// `early_exit_on_iffw` is on (constant at gas model v15+): any IFFW among the early errors
/// short-circuits, even when it is not the head error.
fn should_short_circuit_insufficient_funds(execution_params: &ExecutionOrEarlyError<'_>) -> bool {
    any_error_is_insufficient_funds_for_withdraw(execution_params)
}

/// `GasData::is_unmetered`.
fn is_unmetered(gas_data: &GasData<'_>) -> bool {
    gas_data.payment.len() == 1
        && gas_data.payment[0].id == ObjectId::from_u16(0)
        && gas_data.payment[0].version.get() == 0
        && gas_data.payment[0].digest == Digest::ZERO
}

/// `TransactionKind::is_system_tx`.
fn is_system_tx(transaction_kind: &TransactionKind<'_>) -> bool {
    // Keep this as an exhaustive match so that we can't forget to update it.
    match transaction_kind {
        TransactionKind::ChangeEpoch(_)
        | TransactionKind::Genesis(_)
        | TransactionKind::ConsensusCommitPrologue(_)
        | TransactionKind::ConsensusCommitPrologueV2(_)
        | TransactionKind::ConsensusCommitPrologueV3(_)
        | TransactionKind::ConsensusCommitPrologueV4(_)
        | TransactionKind::AuthenticatorStateUpdate(_)
        | TransactionKind::RandomnessStateUpdate(_)
        | TransactionKind::EndOfEpochTransaction(_)
        | TransactionKind::ProgrammableSystemTransaction(_) => true,
        TransactionKind::ProgrammableTransaction(_) => false,
    }
}

/// `TransactionKind::is_end_of_epoch_tx`.
fn is_end_of_epoch_tx(transaction_kind: &TransactionKind<'_>) -> bool {
    matches!(
        transaction_kind,
        TransactionKind::EndOfEpochTransaction(_) | TransactionKind::ChangeEpoch(_)
    )
}

fn payment_kind<'a>(
    bump: &'a Bump,
    gas_data: &GasData<'a>,
    transaction_kind: &TransactionKind<'a>,
) -> Result<PaymentKind<'a>, ExecutionError<'a>> {
    Ok(
        if is_unmetered(gas_data) || is_system_tx(transaction_kind) {
            PaymentKind::unmetered()
        } else if is_gasless_transaction(gas_data, transaction_kind) {
            PaymentKind::gasless()
        } else if gas_data.payment.is_empty() {
            PaymentKind::smash(
                bump,
                &[PaymentMethod::AddressBalance(
                    *gas_data.owner,
                    gas_data.budget,
                )],
            )
            .ok_or_else(|| {
                ExecutionError::invariant_violation(
                    "unable to create a payment kind with a single address balance",
                )
            })?
        } else {
            let mut payment_methods = Vec::with_capacity_in(gas_data.payment.len(), bump);
            payment_methods.extend(gas_data.payment.iter().map(|entry| {
                // `ParsedDigest::try_from`: a coin reservation's digest carries its amount.
                if entry.is_coin_reservation() {
                    let amount = u64::from_le_bytes(
                        entry.digest.bytes[0..8]
                            .try_into()
                            .expect("a digest has eight bytes"),
                    );
                    PaymentMethod::AddressBalance(*gas_data.owner, amount)
                } else {
                    PaymentMethod::Coin(exec_types::base::object_ref(entry))
                }
            }));
            PaymentKind::smash(bump, &payment_methods).ok_or_else(|| {
                ExecutionError::invariant_violation(
                    "unable to create a payment kind from the gas payment: \
                     duplicate gas coin or reservation overflow",
                )
            })?
        },
    )
}

/// Everything `execute_transaction_to_effects` hands back to the executor layer.
pub struct ExecutionOutput<'a> {
    pub inner_store: InnerTemporaryStore<'a>,
    pub gas_status: SuiGasStatus,
    pub effects: Built<'a>,
    pub timings: Vec<'a, ExecutionTiming>,
    pub execution_result: Result<(), ExecutionError<'a>>,
}

/// Gas summary, execution result, and timings produced by `execute_transaction`.
struct ExecutionOutcome<'a> {
    cost_summary: GasCostSummary,
    execution_result: Result<(), ExecutionError<'a>>,
    timings: Vec<'a, ExecutionTiming>,
}

/// # Panics
/// Below gas model 15, whose legacy path this executor does not implement.
pub fn execute_transaction_to_effects<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    inputs: &'a ExecutionInputs<'a>,
    unsettled_object_funds: &'a dyn UnsettledObjectFundsRead,
    gas_data: GasData<'a>,
    gas_status: SuiGasStatus,
    transaction_kind: TransactionKind<'a>,
    rewritten_inputs: Option<&'a [bool]>,
    transaction_signer: SuiAddress,
    transaction_digest: TransactionDigest,
    move_vm: &Arc<MoveRuntime>,
    epoch_id: &EpochId,
    epoch_timestamp_ms: u64,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    enable_expensive_checks: bool,
    execution_params: ExecutionOrEarlyError<'a>,
    // The epoch's natives cost table, built from `protocol_config`; `None` builds one for this
    // transaction.
    natives_cost_table: Option<&natives::NativesCostTable>,
    // The epoch's types and layouts, which only user transactions use; `None` asks the VM for all.
    type_cache: Option<&Arc<SPT::type_cache::TypeCache>>,
) -> ExecutionOutput<'a> {
    let options = SPT::ExecuteOptions {
        // Only the ownership invariant check, run with the expensive checks, reads it.
        record_invariant_bookkeeping: enable_expensive_checks,
        natives_cost_table,
        type_cache,
    };
    let shared_object_refs = inputs.filter_shared_objects(bump);
    let mut transaction_dependencies = if protocol_config.disable_effects_tx_dependencies() {
        Vec::new_in(bump)
    } else {
        inputs.transaction_dependencies(bump)
    };

    assert!(
        crate::gas_model::gas_predicates::bump_only_enabled(protocol_config.gas_model_version()),
        "gas model {} predates this executor",
        protocol_config.gas_model_version()
    );

    let mut temporary_store = TemporaryStore::new(
        bump,
        store,
        inputs,
        transaction_digest,
        protocol_config,
        *epoch_id,
        (&transaction_kind, &gas_data, transaction_signer),
        unsettled_object_funds,
    );

    let Finalized {
        gas,
        status,
        timings,
        execution_result,
    } = match execute_transaction_to_outcome::<Mode>(
        bump,
        store,
        &mut temporary_store,
        gas_data,
        gas_status,
        transaction_kind,
        rewritten_inputs,
        transaction_signer,
        transaction_digest,
        move_vm,
        epoch_id,
        epoch_timestamp_ms,
        protocol_config,
        metrics,
        enable_expensive_checks,
        execution_params,
        options,
    ) {
        Outcome::Proceed {
            gas_charger,
            gas_cost_summary,
            execution_result,
            timings,
        } => {
            let status = if let Err(error) = &execution_result {
                execution_status(error)
            } else {
                ExecutionStatus::Success
            };
            let coin = gas_charger.gas_coin();
            Finalized {
                gas: GasOutcome {
                    cost_summary: gas_cost_summary,
                    coin,
                    status: gas_charger.into_gas_status(),
                },
                status,
                timings,
                execution_result,
            }
        }
        Outcome::BumpOnly {
            gas_status,
            error,
            reason,
        } => {
            report_bump_only::<Mode>(reason, &transaction_digest, &error);
            // Rebuild the store from its inputs, keeping only the input version bumps.
            temporary_store = temporary_store.into_bump_only();
            Finalized {
                gas: GasOutcome {
                    cost_summary: GasCostSummary::default(),
                    coin: None,
                    status: gas_status,
                },
                status: execution_status(&error),
                timings: Vec::new_in(bump),
                execution_result: Err(error),
            }
        }
    };

    // Shared infallible tail: trim the genesis dependency, build effects, telemetry.
    let GasOutcome {
        cost_summary,
        coin,
        status: gas_status,
    } = gas;
    // `TransactionDigest::genesis_marker`
    transaction_dependencies.retain(|d| *d != Digest::ZERO);
    let (inner, effects) = temporary_store.into_effects(
        &shared_object_refs,
        &transaction_digest,
        transaction_dependencies,
        cost_summary,
        status,
        coin,
        *epoch_id,
    );
    // The reference updates the VM's telemetry metrics here; anchovy does not export them.
    ExecutionOutput {
        inner_store: inner,
        gas_status,
        effects,
        timings,
        execution_result,
    }
}

/// `ExecutionStatus::new_failure(error.to_execution_failure())`.
fn execution_status<'a>(error: &ExecutionError<'a>) -> ExecutionStatus<'a> {
    let (error, command) = error.to_execution_status();
    ExecutionStatus::Failure {
        error,
        command: command.map(|c| c as u64),
    }
}

/// Post-execution consistency: SUI conservation, the expensive ownership invariants, and (on
/// successful execution) the published-packages invariant. `Err` means an invariant was
/// violated unrecoverably - no panic, no recovery; the caller bails to `BumpOnly` reporting
/// the error.
fn check_consistency<'a, Mode: ExecutionMode>(
    temporary_store: &mut TemporaryStore<'a>,
    gas_charger: &GasCharger<'a>,
    gas_cost_summary: &GasCostSummary,
    move_vm: &Arc<MoveRuntime>,
    enable_expensive_checks: bool,
    transaction_signer: SuiAddress,
    sponsor: Option<SuiAddress>,
    is_epoch_change: bool,
    execution_succeeded: bool,
) -> Result<(), (ExecutionError<'a>, BumpOnlyReason)> {
    // FIXME: we cannot fail the transaction if this is an epoch change transaction.
    run_conservation_checks::<Mode>(
        temporary_store,
        move_vm,
        enable_expensive_checks,
        gas_cost_summary,
    )
    .map_err(|error| (error, BumpOnlyReason::Conservation))?;

    // Ownership invariants - only under expensive checks + non-arbitrary mode; a violation is a
    // real bug that should never fire.
    if enable_expensive_checks
        && !Mode::allow_arbitrary_function_calls()
        && temporary_store
            .check_ownership_invariants(&transaction_signer, &sponsor, gas_charger, is_epoch_change)
            .is_err()
    {
        return Err((
            ExecutionError::from_kind(ExecutionErrorKind::InvariantViolation),
            BumpOnlyReason::Ownership,
        ));
    }

    // Written packages must match the PTB's publish/upgrade commands; only meaningful when
    // execution succeeded (on failure the writes were dropped).
    if execution_succeeded {
        temporary_store
            .check_published_packages()
            .map_err(|error| (error, BumpOnlyReason::PublishedPackages))?;
    }

    Ok(())
}

/// `execute_genesis_state_update`: runs `pt` unmetered over a store with no inputs.
pub fn execute_genesis_state_update<'a>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    inputs: &'a ExecutionInputs<'a>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    move_vm: &Arc<MoveRuntime>,
    tx_context: Rc<RefCell<TxContext>>,
    pt: messages::transaction::ProgrammableTransaction<'a>,
) -> Result<InnerTemporaryStore<'a>, ExecutionError<'a>> {
    let mut temporary_store = TemporaryStore::new_for_genesis_state_update(
        bump,
        store,
        inputs,
        tx_context.borrow().digest(),
        protocol_config,
    );
    let mut gas_charger = GasCharger::new_unmetered(tx_context.borrow().digest(), protocol_config);
    SPT::execute::<execution_mode::Genesis>(
        bump,
        protocol_config,
        metrics,
        move_vm,
        &mut temporary_store,
        store,
        tx_context,
        &mut gas_charger,
        None,
        pt,
    )
    .map_err(|(e, _)| e)?;
    temporary_store.update_object_version_and_prev_tx();
    Ok(temporary_store.into_inner(containers::BTreeMap::new_in(bump)))
}

#[allow(clippy::large_enum_variant)]
enum Outcome<'a> {
    Proceed {
        gas_charger: GasCharger<'a>,
        gas_cost_summary: GasCostSummary,
        execution_result: Result<(), ExecutionError<'a>>,
        timings: Vec<'a, ExecutionTiming>,
    },
    BumpOnly {
        gas_status: SuiGasStatus,
        error: ExecutionError<'a>,
        reason: BumpOnlyReason,
    },
}

/// Which stage bailed to the `BumpOnly` exit. `InsufficientFundsForWithdraw` is
/// the one expected reason; every other variant is an execution bug and is reported to
/// `execution_bump_only_exits`, whose `reason` label is `Self::label`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BumpOnlyReason {
    InsufficientFundsForWithdraw,
    GasSmash,
    WriteReset,
    Conservation,
    Ownership,
    PublishedPackages,
}

impl BumpOnlyReason {
    /// Metric label. Alerts select on these, so keep the values stable.
    fn label(self) -> &'static str {
        match self {
            Self::InsufficientFundsForWithdraw => "insufficient_funds_for_withdraw",
            Self::GasSmash => "gas_smash",
            Self::WriteReset => "write_reset",
            Self::Conservation => "conservation",
            Self::Ownership => "ownership",
            Self::PublishedPackages => "published_packages",
        }
    }

    fn is_expected(self) -> bool {
        matches!(self, Self::InsufficientFundsForWithdraw)
    }
}

struct GasOutcome {
    cost_summary: GasCostSummary,
    coin: Option<ObjectId>,
    status: SuiGasStatus,
}

struct Finalized<'a> {
    gas: GasOutcome,
    status: ExecutionStatus<'a>,
    timings: Vec<'a, ExecutionTiming>,
    execution_result: Result<(), ExecutionError<'a>>,
}

/// Report an unexpected `BumpOnly` exit: a transaction whose writes were all dropped and which
/// was charged nothing because a stage of the pipeline failed.
///
/// Skipped for the expected IFFW short-circuit, and for the simulation paths
/// (`Mode::TRACK_EXECUTION`: dev-inspect / dry-run / simulate), where an arbitrary user-supplied
/// transaction must not be able to crash a debug node or raise an alert.
fn report_bump_only<Mode: ExecutionMode>(
    reason: BumpOnlyReason,
    transaction_digest: &TransactionDigest,
    error: &ExecutionError<'_>,
) {
    if reason.is_expected() || Mode::TRACK_EXECUTION {
        return;
    }
    // The reference's `debug_fatal_with_metric!`: panics in debug builds, and counts in
    // `execution_bump_only_exits`, which anchovy does not export.
    debug_assert!(
        false,
        "BumpOnly exit: all writes dropped, no gas charged. \
         reason={}, tx_digest={:?}, error={:?}",
        reason.label(),
        transaction_digest,
        error
    );
}

fn execute_transaction_to_outcome<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    temporary_store: &mut TemporaryStore<'a>,
    gas_data: GasData<'a>,
    gas_status: SuiGasStatus,
    transaction_kind: TransactionKind<'a>,
    rewritten_inputs: Option<&'a [bool]>,
    transaction_signer: SuiAddress,
    transaction_digest: TransactionDigest,
    move_vm: &Arc<MoveRuntime>,
    epoch_id: &EpochId,
    epoch_timestamp_ms: u64,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    enable_expensive_checks: bool,
    execution_params: ExecutionOrEarlyError<'a>,
    options: SPT::ExecuteOptions<'_>,
) -> Outcome<'a> {
    // Short-circuit insufficient_funds. No execution, `Outcome::BumpOnly`
    if should_short_circuit_insufficient_funds(&execution_params) {
        let iffw = ExecutionError::from_kind(ExecutionErrorKind::InsufficientFundsForWithdraw);
        return Outcome::BumpOnly {
            gas_status,
            error: iffw,
            reason: BumpOnlyReason::InsufficientFundsForWithdraw,
        };
    }

    let sponsor = {
        let gas_owner = *gas_data.owner;
        if gas_owner == transaction_signer {
            None
        } else {
            Some(gas_owner)
        }
    };
    let gas_price = gas_status.gas_price();
    let rgp = gas_status.reference_gas_price();
    let is_epoch_change = is_end_of_epoch_tx(&transaction_kind);

    let tx_ctx = TxContext::new_from_components(
        &transaction_signer,
        &transaction_digest,
        epoch_id,
        epoch_timestamp_ms,
        rgp,
        gas_price,
        gas_data.budget,
        sponsor,
        protocol_config,
    );
    let tx_ctx = Rc::new(RefCell::new(tx_ctx));

    let payment_kind = match payment_kind(bump, &gas_data, &transaction_kind) {
        Ok(payment_kind) => payment_kind,
        Err(error) => {
            return Outcome::BumpOnly {
                gas_status,
                error,
                reason: BumpOnlyReason::GasSmash,
            };
        }
    };
    let mut gas_charger = GasCharger::new(
        transaction_digest,
        payment_kind,
        gas_status,
        temporary_store,
        protocol_config,
    );
    let ExecutionOutcome {
        cost_summary: gas_cost_summary,
        execution_result,
        timings,
    } = match execute_transaction::<Mode>(
        bump,
        store,
        temporary_store,
        transaction_kind,
        rewritten_inputs,
        &mut gas_charger,
        tx_ctx,
        move_vm,
        protocol_config,
        metrics,
        execution_params,
        options,
    ) {
        Err((error, reason)) => {
            return Outcome::BumpOnly {
                gas_status: gas_charger.into_gas_status(),
                error,
                reason,
            };
        }
        Ok(outcome) => outcome,
    };

    // Post-execution consistency (conservation + ownership + published packages): on violation
    // bail to `BumpOnly` with the gas_status recovered from the charger
    match check_consistency::<Mode>(
        temporary_store,
        &gas_charger,
        &gas_cost_summary,
        move_vm,
        enable_expensive_checks,
        transaction_signer,
        sponsor,
        is_epoch_change,
        execution_result.is_ok(),
    ) {
        Ok(()) => Outcome::Proceed {
            gas_charger,
            gas_cost_summary,
            execution_result,
            timings,
        },
        Err((error, reason)) => Outcome::BumpOnly {
            gas_status: gas_charger.into_gas_status(),
            error,
            reason,
        },
    }
}

fn execute_transaction<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    temporary_store: &mut TemporaryStore<'a>,
    transaction_kind: TransactionKind<'a>,
    rewritten_inputs: Option<&'a [bool]>,
    gas_charger: &mut GasCharger<'a>,
    tx_ctx: Rc<RefCell<TxContext>>,
    move_vm: &Arc<MoveRuntime>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    execution_params: ExecutionOrEarlyError<'a>,
    options: SPT::ExecuteOptions<'_>,
) -> Result<ExecutionOutcome<'a>, (ExecutionError<'a>, BumpOnlyReason)> {
    debug_assert!(
        gas_charger.no_charges(),
        "No gas charges must be applied yet"
    );

    let mut timings = Vec::new_in(bump);

    let result = gas_charger
        .charge_input_objects(temporary_store)
        // Early errors fail without running the VM
        .and_then(|()| match execution_params.head() {
            Some(head) => Err(ExecutionError::new(head, None)),
            None => execute_ptb::<Mode>(
                bump,
                store,
                temporary_store,
                transaction_kind,
                rewritten_inputs,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics.clone(),
                &mut timings,
                options,
            ),
        })
        .and_then(|v| gas_charger.meter_storage(temporary_store).map(|_| v));

    let checks = check_effects(temporary_store, gas_charger, protocol_config, metrics);
    // Execution error wins; otherwise a failed effects check fails the tx.
    let result = result.and_then(|v| checks.map(|()| v));

    if result.is_err() {
        gas_charger
            .handle_error(temporary_store)
            .map_err(|error| (error, BumpOnlyReason::WriteReset))?;
    }
    let cost_summary = gas_charger.charge(temporary_store, &result);
    Ok(ExecutionOutcome {
        cost_summary,
        execution_result: result,
        timings,
    })
}

/// Execute the PTB, then bucketize computation via `round_computation`. Timings are written to
/// `timings_out` regardless of Ok/Err.
fn execute_ptb<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    temporary_store: &mut TemporaryStore<'a>,
    transaction_kind: TransactionKind<'a>,
    rewritten_inputs: Option<&'a [bool]>,
    tx_ctx: Rc<RefCell<TxContext>>,
    move_vm: &Arc<MoveRuntime>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    timings_out: &mut Vec<'a, ExecutionTiming>,
    options: SPT::ExecuteOptions<'_>,
) -> Result<(), ExecutionError<'a>> {
    let result = match execution_loop::<Mode>(
        bump,
        store,
        temporary_store,
        transaction_kind,
        rewritten_inputs,
        tx_ctx,
        move_vm,
        gas_charger,
        protocol_config,
        metrics,
        options,
    ) {
        Ok((v, t)) => {
            *timings_out = t;
            Ok(v)
        }
        Err((e, t)) => {
            *timings_out = t;
            Err(e)
        }
    };
    gas_charger.round_computation(result)
}

fn check_effects<'a>(
    temporary_store: &mut TemporaryStore<'a>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
) -> Result<(), ExecutionError<'a>> {
    let meter = check_meter_limit(
        temporary_store,
        gas_charger,
        protocol_config,
        metrics.clone(),
    );
    let written =
        check_written_objects_limit(temporary_store, gas_charger, protocol_config, metrics);
    let representable = temporary_store.check_accumulator_amounts_representable();
    meter.and(written).and(representable)
}

fn run_conservation_checks<'a, Mode: ExecutionMode>(
    temporary_store: &mut TemporaryStore<'a>,
    move_vm: &Arc<MoveRuntime>,
    enable_expensive_checks: bool,
    cost_summary: &GasCostSummary,
) -> Result<(), ExecutionError<'a>> {
    // The reference logs the failure before returning it.
    temporary_store.check_conservation_invariants::<Mode>(
        move_vm,
        enable_expensive_checks,
        cost_summary,
    )
}

fn check_meter_limit<'a>(
    temporary_store: &mut TemporaryStore<'a>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
) -> Result<(), ExecutionError<'a>> {
    let effects_estimated_size = temporary_store.estimate_effects_size_upperbound();

    // Check if a limit threshold was crossed.
    // For metered transactions, there is not soft limit.
    // For system transactions, we allow a soft limit with alerting, and a hard limit where we terminate
    match check_limit_by_meter!(
        !gas_charger.is_unmetered(),
        effects_estimated_size,
        protocol_config.max_serialized_tx_effects_size_bytes(),
        protocol_config.max_serialized_tx_effects_size_bytes_system_tx(),
        metrics.limits_metrics.excessive_estimated_effects_size
    ) {
        LimitThresholdCrossed::None => Ok(()),
        // The reference logs a warning.
        LimitThresholdCrossed::Soft(_, _) => Ok(()),
        LimitThresholdCrossed::Hard(_, lim) => Err(ExecutionError::new_with_source(
            ExecutionErrorKind::EffectsTooLarge {
                current_size: effects_estimated_size as u64,
                max_size: lim as u64,
            },
            "Transaction effects are too large",
        )),
    }
}

fn check_written_objects_limit<'a>(
    temporary_store: &mut TemporaryStore<'a>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
) -> Result<(), ExecutionError<'a>> {
    if let (Some(normal_lim), Some(system_lim)) = (
        protocol_config.max_size_written_objects_as_option(),
        protocol_config.max_size_written_objects_system_tx_as_option(),
    ) {
        let written_objects_size = temporary_store.written_objects_size();

        match check_limit_by_meter!(
            !gas_charger.is_unmetered(),
            written_objects_size,
            normal_lim,
            system_lim,
            metrics.limits_metrics.excessive_written_objects_size
        ) {
            LimitThresholdCrossed::None => (),
            // The reference logs a warning.
            LimitThresholdCrossed::Soft(_, _) => (),
            LimitThresholdCrossed::Hard(_, lim) => {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::WrittenObjectsTooLarge {
                        current_size: written_objects_size as u64,
                        max_size: lim as u64,
                    },
                    "Written objects size crossed hard limit",
                ));
            }
        };
    }

    Ok(())
}

fn execution_loop<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    store: &'a dyn BackingStore<'a>,
    temporary_store: &mut TemporaryStore<'a>,
    transaction_kind: TransactionKind<'a>,
    rewritten_inputs: Option<&'a [bool]>,
    tx_ctx: Rc<RefCell<TxContext>>,
    move_vm: &Arc<MoveRuntime>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    options: SPT::ExecuteOptions<'_>,
) -> ResultWithTimings<'a, (), ExecutionError<'a>> {
    let no_timings = || Vec::new_in(bump);
    let result = match transaction_kind {
        TransactionKind::Genesis(objects) => {
            if tx_ctx.borrow().epoch() != 0 {
                panic!("BUG: Genesis Transactions can only be executed in epoch 0");
            }

            for genesis_object in objects {
                let object = Object::new_from_data(
                    genesis_object.data,
                    genesis_object.owner,
                    tx_ctx.borrow().digest(),
                );
                temporary_store.create_object(object);
            }
            Ok(((), no_timings()))
        }
        TransactionKind::ConsensusCommitPrologue(prologue) => {
            setup_consensus_commit(
                bump,
                prologue.commit_timestamp_ms,
                temporary_store,
                store,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics,
                options,
            )
            .expect("ConsensusCommitPrologue cannot fail");
            Ok(((), no_timings()))
        }
        TransactionKind::ConsensusCommitPrologueV2(prologue) => {
            setup_consensus_commit(
                bump,
                prologue.commit_timestamp_ms,
                temporary_store,
                store,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics,
                options,
            )
            .expect("ConsensusCommitPrologueV2 cannot fail");
            Ok(((), no_timings()))
        }
        TransactionKind::ConsensusCommitPrologueV3(prologue) => {
            setup_consensus_commit(
                bump,
                prologue.get().commit_timestamp_ms,
                temporary_store,
                store,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics,
                options,
            )
            .expect("ConsensusCommitPrologueV3 cannot fail");
            Ok(((), no_timings()))
        }
        TransactionKind::ConsensusCommitPrologueV4(prologue) => {
            setup_consensus_commit(
                bump,
                prologue.get().commit_timestamp_ms,
                temporary_store,
                store,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics,
                options,
            )
            .expect("ConsensusCommitPrologue cannot fail");
            Ok(((), no_timings()))
        }
        TransactionKind::ProgrammableTransaction(pt) => SPT::execute_with_options::<Mode>(
            bump,
            protocol_config,
            metrics,
            move_vm,
            temporary_store,
            store,
            tx_ctx,
            gas_charger,
            rewritten_inputs,
            pt,
            options,
        ),
        TransactionKind::ProgrammableSystemTransaction(pt) => {
            SPT::execute_with_options::<execution_mode::System>(
                bump,
                protocol_config,
                metrics,
                move_vm,
                temporary_store,
                store,
                tx_ctx,
                gas_charger,
                None,
                pt,
                SPT::ExecuteOptions {
                    type_cache: None,
                    ..options
                },
            )
            .map_err(|(e, _)| (e, no_timings()))?;
            Ok(((), no_timings()))
        }
        TransactionKind::RandomnessStateUpdate(randomness_state_update) => {
            setup_randomness_state_update(
                bump,
                randomness_state_update.get(),
                temporary_store,
                store,
                tx_ctx,
                move_vm,
                gas_charger,
                protocol_config,
                metrics,
                options,
            )
            .map_err(|e| (e, no_timings()))?;
            Ok(((), no_timings()))
        }
        // The epoch change, the end-of-epoch transactions and authenticator state updates come
        // with the system transactions' port.
        TransactionKind::ChangeEpoch(_)
        | TransactionKind::EndOfEpochTransaction(_)
        | TransactionKind::AuthenticatorStateUpdate(_) => Err((
            ExecutionError::invariant_violation(
                "epoch change and authenticator state transactions are not yet executed",
            ),
            no_timings(),
        )),
    }?;
    temporary_store
        .check_execution_results_consistency()
        .map_err(|e| (e, no_timings()))?;
    Ok(result)
}

/// Perform metadata updates in preparation for the transactions in the upcoming checkpoint:
///
/// - Set the timestamp for the `Clock` shared object from the timestamp in the header from
///   consensus.
fn setup_consensus_commit<'a>(
    bump: &'a Bump,
    consensus_commit_timestamp_ms: u64,
    temporary_store: &mut TemporaryStore<'a>,
    store: &'a dyn BackingStore<'a>,
    tx_ctx: Rc<RefCell<TxContext>>,
    move_vm: &Arc<MoveRuntime>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    options: SPT::ExecuteOptions<'_>,
) -> Result<(), ExecutionError<'a>> {
    let pt = {
        let mut builder = ProgrammableTransactionBuilder::new(bump);
        let timestamp = builder.bcs(&consensus_commit_timestamp_ms);
        let res = builder.move_call(
            exec_types::base::object_id(&move_core_types::account_address::AccountAddress::TWO),
            "clock",
            "consensus_commit_prologue",
            &[],
            &[
                CallArg::Object(ObjectArg::SharedObject(&CLOCK_MUT)),
                CallArg::Pure(timestamp),
            ],
        );
        assert_invariant!(
            res.is_ok(),
            "Unable to generate consensus_commit_prologue transaction!"
        );
        builder.finish()
    };
    SPT::execute_with_options::<execution_mode::System>(
        bump,
        protocol_config,
        metrics,
        move_vm,
        temporary_store,
        store,
        tx_ctx,
        gas_charger,
        None,
        pt,
        SPT::ExecuteOptions {
            type_cache: None,
            ..options
        },
    )
    .map_err(|(e, _)| e)?;
    Ok(())
}

fn setup_randomness_state_update<'a>(
    bump: &'a Bump,
    update: &messages::system_transaction::RandomnessStateUpdate<'a>,
    temporary_store: &mut TemporaryStore<'a>,
    store: &'a dyn BackingStore<'a>,
    tx_ctx: Rc<RefCell<TxContext>>,
    move_vm: &Arc<MoveRuntime>,
    gas_charger: &mut GasCharger<'a>,
    protocol_config: &'a ProtocolConfig,
    metrics: Arc<ExecutionMetrics>,
    options: SPT::ExecuteOptions<'_>,
) -> Result<(), ExecutionError<'a>> {
    let pt = {
        let mut builder = ProgrammableTransactionBuilder::new(bump);
        let randomness = containers::alloc(
            bump,
            messages::transaction::SharedObjectArg::new(
                SUI_RANDOMNESS_STATE_OBJECT_ID,
                update.randomness_obj_initial_shared_version,
                SharedObjectMutability::Mutable,
            ),
        );
        let round = builder.bcs(&update.randomness_round);
        let random_bytes = builder.bcs(update.random_bytes);
        let res = builder.move_call(
            exec_types::base::object_id(&move_core_types::account_address::AccountAddress::TWO),
            "random",
            "update_randomness_state",
            &[],
            &[
                CallArg::Object(ObjectArg::SharedObject(randomness)),
                CallArg::Pure(round),
                CallArg::Pure(random_bytes),
            ],
        );
        assert_invariant!(
            res.is_ok(),
            "Unable to generate randomness_state_update transaction!"
        );
        builder.finish()
    };
    SPT::execute_with_options::<execution_mode::System>(
        bump,
        protocol_config,
        metrics,
        move_vm,
        temporary_store,
        store,
        tx_ctx,
        gas_charger,
        None,
        pt,
        SPT::ExecuteOptions {
            type_cache: None,
            ..options
        },
    )
    .map_err(|(e, _)| e)?;
    Ok(())
}
