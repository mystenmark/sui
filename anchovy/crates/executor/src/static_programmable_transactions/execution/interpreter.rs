// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Move tracing is not ported (see `context`), and no mode tracks results (dev-inspect is left
//! out), so `execute` returns `()` with its timings.

use crate::{
    error::{ExecutionError, ExecutionErrorKind},
    execution::{ExecutionTiming, ResultWithTimings},
    execution_mode::ExecutionMode,
    gas_charger::GasCharger,
    object_runtime, sp,
    static_programmable_transactions::{
        ExecuteOptions,
        env::Env,
        execution::context::{Context, CtxValue, GasCoinTransfer},
        typing::{ast as T, verify::input_arguments::is_coin_send_funds},
    },
};
use containers::{BTreeMap, Bump, Vec, alloc, alloc_slice_copy};
use exec_types::assert_invariant;
use exec_types::tx_context::TxContext;
use messages::base::SuiAddress;
use messages::execution_status::PackageUpgradeError;
use messages::object::Owner;
use move_core_types::account_address::AccountAddress;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use sui_types::{fp_ensure, metrics::ExecutionMetrics};

pub fn execute<'env, 'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode: ExecutionMode>(
    env: &'env mut Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
    metrics: Arc<ExecutionMetrics>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger<'a>,
    ast: T::Transaction<'a>,
    options: ExecuteOptions<'_>,
) -> ResultWithTimings<'a, (), ExecutionError<'a>>
where
    'pc: 'a,
{
    let bump = env.bump;
    let mut indexed_timings = IndexedExecutionTimings::new(bump, ast.original_command_len);
    let result = execute_inner::<Mode>(
        &mut indexed_timings,
        env,
        metrics,
        tx_context,
        gas_charger,
        ast,
        options,
    );
    let timings = indexed_timings.into_coalesced();

    match result {
        Ok(result) => Ok((result, timings)),
        Err(e) => Err((e, timings)),
    }
}

fn execute_inner<'env, 'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode: ExecutionMode>(
    timings: &mut IndexedExecutionTimings<'a>,
    env: &'env mut Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
    metrics: Arc<ExecutionMetrics>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger<'a>,
    ast: T::Transaction<'a>,
    options: ExecuteOptions<'_>,
) -> Result<(), ExecutionError<'a>>
where
    'pc: 'a,
{
    debug_assert_eq!(gas_charger.move_gas_status().stack_height_current(), 0);
    let T::Transaction {
        gas_payment,
        bytes,
        objects,
        withdrawals,
        pure,
        receiving,
        withdrawal_compatibility_conversions: _,
        original_command_len: _,
        commands,
        unified_linkage: _,
    } = ast;
    let mut context = Context::new(
        env,
        metrics,
        tx_context,
        gas_charger,
        gas_payment,
        bytes,
        objects,
        withdrawals,
        pure,
        receiving,
        options.natives_cost_table,
    )?;

    for sp!(annotated_index, c) in commands {
        let annotated_index = annotated_index as usize;
        let start = Instant::now();
        if let Err(err) = execute_command::<Mode>(&mut context, c) {
            // We still need to record the loaded child objects for replay
            let loaded_runtime_objects = object_runtime!(context)?.loaded_runtime_objects();
            // we do not save the wrapped objects since on error, they should not be modified
            drop(context);
            env.state_view
                .save_loaded_runtime_objects(loaded_runtime_objects);
            timings.error(annotated_index, start.elapsed());
            return Err(err.with_command_index(annotated_index));
        };
        timings.executed(annotated_index, start.elapsed());
    }
    // Save loaded objects table in case we fail in post execution
    //
    // We still need to record the loaded child objects for replay
    // Record the objects loaded at runtime (dynamic fields + received) for
    // storage rebate calculation.
    let loaded_runtime_objects = object_runtime!(context)?.loaded_runtime_objects();
    // Only the ownership invariant check reads these, so they are left out when it does not run.
    let invariant_bookkeeping = if options.record_invariant_bookkeeping {
        // We record what objects were contained in at the start of the transaction
        // for expensive invariant checks
        let wrapped_object_containers = object_runtime!(context)?.wrapped_object_containers();
        // We record the generated object IDs for expensive invariant checks
        let generated_object_ids = object_runtime!(context)?.generated_object_ids();
        Some((wrapped_object_containers, generated_object_ids))
    } else {
        None
    };

    // apply changes
    let finished = context.finish();
    // Save loaded objects for debug. We dont want to lose the info
    env.state_view
        .save_loaded_runtime_objects(loaded_runtime_objects);
    let generated_object_ids = invariant_bookkeeping.map(|(wrapped, generated)| {
        env.state_view.save_wrapped_object_containers(wrapped);
        generated
    });
    env.state_view.record_execution_results(finished?)?;
    if let Some(generated_object_ids) = generated_object_ids {
        env.state_view
            .record_generated_object_ids(generated_object_ids);
    }
    Ok(())
}

/// Execute a single command
fn execute_command<'a, Mode: ExecutionMode>(
    context: &mut Context<'_, 'a, '_, '_, '_, '_, '_, '_, Mode>,
    c: T::Command_<'a>,
) -> Result<(), ExecutionError<'a>> {
    let bump: &'a Bump = context.env.bump;
    let T::Command_ {
        command,
        result_type: _,
        drop_values,
        incurs_post_execution_checks: _,
    } = c;
    assert_invariant!(
        context.gas_charger.move_gas_status().stack_height_current() == 0,
        "stack height did not start at 0"
    );
    let is_move_call = matches!(command, T::Command__::MoveCall(_));
    let num_args = command.arguments_len();
    let mut args_to_update = Vec::new_in(bump);
    let result = match command {
        T::Command__::MoveCall(move_call) => {
            let T::MoveCall {
                function,
                arguments,
            } = containers::Box::into_inner(move_call);
            // Detect send_funds with gas coin
            let is_gas_coin_send_funds = is_coin_send_funds(&function)
                && arguments.first().is_some_and(|arg| {
                    matches!(
                        &arg.value.0,
                        T::Argument__::Use(T::Usage::Move(T::Location::GasCoin))
                    )
                });
            if Mode::TRACK_EXECUTION {
                args_to_update.extend(
                    arguments
                        .iter()
                        .filter(|arg| matches!(&arg.value.1, T::Type::Reference(/* mut */ true, _)))
                        .cloned(),
                )
            }
            let arguments: Vec<CtxValue> = context.arguments(arguments)?;
            if is_gas_coin_send_funds {
                assert_invariant!(arguments.len() == 2, "coin::send_funds should have 2 args");
                let recipient = arguments.last().unwrap().to_address()?;
                context.record_gas_coin_transfer(GasCoinTransfer::SendFunds { recipient })?;
            }
            context.vm_move_call(function, arguments)?
        }
        T::Command__::TransferObjects(objects, recipient) => {
            // Check if any object is the gas coin moved by value before consuming
            let has_gas_coin_move = objects.iter().any(|arg| {
                matches!(
                    &arg.value.0,
                    T::Argument__::Use(T::Usage::Move(T::Location::GasCoin))
                )
            });
            if has_gas_coin_move {
                context.record_gas_coin_transfer(GasCoinTransfer::TransferObjects)?;
            }
            let mut object_tys = Vec::with_capacity_in(objects.len(), bump);
            object_tys.extend(objects.iter().map(|sp!(_, (_, ty))| *ty));
            let object_values: Vec<CtxValue> = context.arguments(objects)?;
            let recipient: AccountAddress = context.argument(recipient)?;
            assert_invariant!(
                object_values.len() == object_tys.len(),
                "object values and types mismatch"
            );
            // Lengths checked above.
            #[allow(clippy::disallowed_methods)]
            for (object_value, ty) in object_values.into_iter().zip(object_tys) {
                // TODO should we just call a Move function?
                let recipient =
                    Owner::AddressOwner(alloc(bump, SuiAddress(recipient.into_bytes())));
                context.transfer_object(recipient, ty, object_value)?;
            }
            Vec::new_in(bump)
        }
        T::Command__::SplitCoins(_ty, coin, amounts) => {
            // TODO should we just call a Move function?
            if Mode::TRACK_EXECUTION {
                args_to_update.push(coin.clone());
            }
            let coin_ref: CtxValue = context.argument(coin)?;
            let amount_values: Vec<u64> = context.arguments(amounts)?;
            let mut total: u64 = 0;
            for amount in &amount_values {
                let Some(new_total) = total.checked_add(*amount) else {
                    return Err(ExecutionError::from_kind(
                        ExecutionErrorKind::CoinBalanceOverflow,
                    ));
                };
                total = new_total;
            }
            let coin_value = context.copy_value(&coin_ref)?.coin_ref_value()?;
            fp_ensure!(
                coin_value >= total,
                ExecutionError::new_with_source(
                    ExecutionErrorKind::InsufficientCoinBalance,
                    format!("balance: {coin_value} required: {total}")
                )
            );
            coin_ref.coin_ref_subtract_balance(total)?;
            let mut amounts = Vec::with_capacity_in(amount_values.len(), bump);
            for a in amount_values {
                amounts.push(context.new_coin(a)?);
            }
            amounts
        }
        T::Command__::MergeCoins(_ty, target, coins) => {
            // TODO should we just call a Move function?
            if Mode::TRACK_EXECUTION {
                args_to_update.push(target.clone());
            }
            let target_ref: CtxValue = context.argument(target)?;
            let coins = context.arguments(coins)?;
            let mut amounts = Vec::with_capacity_in(coins.len(), bump);
            for coin in coins {
                amounts.push(context.destroy_coin(coin)?);
            }
            let mut additional: u64 = 0;
            for amount in amounts {
                let Some(new_additional) = additional.checked_add(amount) else {
                    return Err(ExecutionError::from_kind(
                        ExecutionErrorKind::CoinBalanceOverflow,
                    ));
                };
                additional = new_additional;
            }
            let target_value = context.copy_value(&target_ref)?.coin_ref_value()?;
            fp_ensure!(
                target_value.checked_add(additional).is_some(),
                ExecutionError::from_kind(ExecutionErrorKind::CoinBalanceOverflow,)
            );
            target_ref.coin_ref_add_balance(additional)?;
            Vec::new_in(bump)
        }
        T::Command__::MakeMoveVec(ty, items) => {
            let items: Vec<CtxValue> = context.arguments(items)?;
            let mut result = Vec::with_capacity_in(1, bump);
            result.push(CtxValue::vec_pack(ty, items)?);
            result
        }
        T::Command__::Publish(payload, dep_ids, linkage) => {
            let package_payload = context.deserialize_package(payload, &dep_ids)?;

            let original_id =
                context.publish_and_init_package(package_payload, &dep_ids, linkage)?;

            if <Mode>::packages_are_predefined() {
                // no upgrade cap for genesis modules
                Vec::new_in(bump)
            } else {
                let mut result = Vec::with_capacity_in(1, bump);
                result.push(context.new_upgrade_cap(original_id)?);
                result
            }
        }
        T::Command__::Upgrade(payload, dep_ids, current_package_id, upgrade_ticket, linkage) => {
            let upgrade_ticket = context
                .argument::<CtxValue>(upgrade_ticket)?
                .into_upgrade_ticket()?;
            // Make sure the passed-in package ID matches the package ID in the `upgrade_ticket`.
            if current_package_id != upgrade_ticket.package {
                return Err(ExecutionError::from_kind(
                    ExecutionErrorKind::PackageUpgradeError {
                        upgrade_error: PackageUpgradeError::PackageIDDoesNotMatch {
                            package_id: alloc(bump, current_package_id),
                            ticket_id: alloc(bump, upgrade_ticket.package),
                        },
                    },
                ));
            }
            // deserialize modules and charge gas
            let package_payload = context.deserialize_package(payload, &dep_ids)?;
            // The reference compares an owned copy of the digest.
            let computed_digest = &package_payload.computed_digest[..];

            if computed_digest != upgrade_ticket.digest {
                return Err(ExecutionError::from_kind(
                    ExecutionErrorKind::PackageUpgradeError {
                        upgrade_error: PackageUpgradeError::DigestDoesNotMatch {
                            digest: alloc_slice_copy(bump, computed_digest),
                        },
                    },
                ));
            }

            let upgraded_package_id = context.upgrade(
                package_payload,
                &dep_ids,
                current_package_id,
                upgrade_ticket.policy,
                linkage,
            )?;

            let mut result = Vec::with_capacity_in(1, bump);
            result.push(context.upgrade_receipt(upgrade_ticket, upgraded_package_id));
            result
        }
    };
    if Mode::TRACK_EXECUTION {
        // No mode tracks results (dev-inspect is left out), so the updates are not recorded.
        let _argument_updates = context.argument_updates(args_to_update)?;
    }
    assert_invariant!(
        result.len() == drop_values.len(),
        "result values and drop values mismatch"
    );
    context.charge_command(is_move_call, num_args, result.len())?;
    let mut kept = Vec::with_capacity_in(result.len(), bump);
    // Lengths checked above.
    #[allow(clippy::disallowed_methods)]
    kept.extend(
        result
            .into_iter()
            .zip(drop_values)
            .map(|(value, drop)| if !drop { Some(value) } else { None }),
    );
    context.result(kept)?;
    assert_invariant!(
        context.gas_charger.move_gas_status().stack_height_current() == 0,
        "stack height did not end at 0"
    );
    Ok(())
}

/// Struct to track execution timings, coalesced into the annotated command indices.
struct IndexedExecutionTimings<'a> {
    bump: &'a Bump,
    /// The number of commands in the original command vector.
    original_command_len: usize,
    /// Mapping from the command's annotated index to its duration. Multiple commands may share
    /// the same annotated index, in which case their durations will be added together.
    executed_commands: BTreeMap<'a, usize, Duration>,
    /// `Some` if an error occurred, stopping execution.
    /// `usize` is the annotated index of the command.
    error_command: Option<(usize, Duration)>,
}

impl<'a> IndexedExecutionTimings<'a> {
    fn new(bump: &'a Bump, original_command_len: usize) -> Self {
        Self {
            bump,
            original_command_len,
            executed_commands: BTreeMap::new_in(bump),
            error_command: None,
        }
    }

    /// The largest index an annotated index may be capped to.
    fn max_allowed_index(&self) -> usize {
        self.original_command_len.saturating_sub(1)
    }

    /// Records the execution of a successful command.
    fn executed(&mut self, annotated_index: usize, duration: Duration) {
        debug_assert!(
            self.error_command.is_none(),
            "command executed after an error occurred"
        );
        let index = annotated_index.min(self.max_allowed_index());
        let existing = self
            .executed_commands
            .entry(index)
            .or_insert(Duration::ZERO);
        *existing = existing.saturating_add(duration);
    }

    /// Record the execution of a failed command that errored and stopped the execution of the PTB.
    fn error(&mut self, annotated_index: usize, duration: Duration) {
        debug_assert!(self.error_command.is_none(), "multiple errors recorded");
        let index = annotated_index.min(self.max_allowed_index());
        debug_assert!(
            self.executed_commands
                .last_key_value()
                .is_none_or(|(last, _)| *last <= index),
            "execution timings recorded for command index {:?} after error at index {}",
            self.executed_commands
                .last_key_value()
                .map(|(last, _)| *last),
            index,
        );

        let existing_opt = self.executed_commands.remove(&index);
        let total_duration = existing_opt
            .unwrap_or(Duration::ZERO)
            .saturating_add(duration);
        self.error_command = Some((index, total_duration));
    }

    /// Coalesces timings by each commands annotated index to align with the original command count.
    /// Extra commands may have been injected during typing (e.g., withdrawal compatibility).
    /// Timings sharing an `annotated_index` have their durations summed. An error, if present,
    /// is always last.
    fn into_coalesced(self) -> Vec<'a, ExecutionTiming> {
        let max_allowed_index = self.max_allowed_index();
        let Self {
            bump,
            original_command_len,
            executed_commands,
            error_command,
        } = self;

        // Injected commands are annotated with the original command they belong to, so with no
        // original commands there is nothing to attribute their timings to.
        if original_command_len == 0 {
            return Vec::new_in(bump);
        }

        let max_executed_index = executed_commands.keys().last().copied();
        let error_index = error_command.as_ref().map(|(idx, _)| *idx);
        let max_used_index = match (max_executed_index, error_index) {
            (Some(exec), Some(err)) => exec.max(err),
            (Some(idx), None) | (None, Some(idx)) => idx,
            (None, None) => return Vec::new_in(bump),
        };
        debug_assert!(
            max_used_index <= max_allowed_index,
            "max used index {} exceeds max allowed index {}",
            max_used_index,
            max_allowed_index
        );
        let size = max_used_index.saturating_add(1);
        debug_assert!(
            size <= original_command_len,
            "coalesced timings length {} exceeds original command length {}",
            size,
            original_command_len
        );

        // We initialize a vector of `Success` timings with zero duration, since we have no
        // guarantee at this point that there are no gaps in the annotated indices. Presently,
        // there should be no gaps, but there is nothing inherent to the annotation scheme that
        // guarantees they are not sparse.
        let mut coalesced = Vec::with_capacity_in(size, bump);
        coalesced.resize(size, ExecutionTiming::Success(Duration::ZERO));
        for (index, duration) in executed_commands {
            let Some(entry) = coalesced.get_mut(index) else {
                debug_assert!(
                    false,
                    "failed to initialize coalesced timings at index {}",
                    index
                );
                continue;
            };
            debug_assert!(matches!(entry, ExecutionTiming::Success(d) if d.is_zero()));
            *entry = ExecutionTiming::Success(duration);
        }

        if let Some((index, error_duration)) = error_command {
            debug_assert!(
                index == coalesced.len().saturating_sub(1),
                "error index should be last"
            );
            if let Some(entry) = coalesced.get_mut(index) {
                debug_assert!(matches!(entry, ExecutionTiming::Success(d) if d.is_zero()));
                *entry = ExecutionTiming::Abort(error_duration);
            } else {
                debug_assert!(
                    false,
                    "failed to initialize coalesced timings at index {}",
                    index
                );
            };
        }

        coalesced
    }
}
