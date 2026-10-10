// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::error::{ExecutionError, ExecutionErrorKind};
use crate::{
    sp,
    static_programmable_transactions::{
        metering::translation_meter::TranslationMeter, typing::ast as T,
    },
};
use exec_types::{assert_invariant, invariant_violation};
use sui_protocol_config::ProtocolConfig;
use sui_types::base_types::TxContextKind;

/// Tracks the references live at any point in the transaction, and the total returned
struct Context<'pc> {
    protocol_config: &'pc ProtocolConfig,
    live: u64,
    total_returned: u64,
}

/// Charges for the references live at each command, and checks three limits:
/// - The number of references live at any point
/// - The number of references returned by any command
/// - The number of references returned over the whole transaction
pub fn meter<'a>(
    meter: &mut TranslationMeter<'_, '_, 'a>,
    protocol_config: &ProtocolConfig,
    transaction: &T::Transaction<'_>,
) -> Result<(), ExecutionError<'a>> {
    let mut context = Context {
        protocol_config,
        live: 0,
        total_returned: 0,
    };
    for (idx, c) in transaction.commands.iter().enumerate() {
        command(&mut context, meter, c).map_err(|e| e.with_command_index(idx))?;
    }
    Ok(())
}

fn command<'a>(
    context: &mut Context,
    meter: &mut TranslationMeter<'_, '_, 'a>,
    sp!(_, c): &T::Command<'_>,
) -> Result<(), ExecutionError<'a>> {
    // The references created by the arguments are held by the command until the return values
    // are fully processed
    let held = arguments(context, c.command.arguments())?;

    let returned = c.result_type.iter().filter(|ty| ty.is_reference()).count() as u64;
    context.returned_n(returned)?;
    // The references held for the arguments are consumed once the command has its return values
    context.free_n(held)?;

    // Unused results are dropped at the end of the command
    // The lengths are checked equal first, which `zip_debug_eq` would also check.
    #[allow(clippy::disallowed_methods)]
    let dropped = {
        assert_invariant!(
            c.drop_values.len() == c.result_type.len(),
            "command drop_values length does not match result_type length"
        );
        c.drop_values
            .iter()
            .zip(&c.result_type)
            .filter(|(drop, ty)| **drop && ty.is_reference())
            .count() as u64
    };
    context.free_n(dropped)?;
    meter.charge_num_live_references(context.live)
}

/// Returns the number of references for the arguments, excluding the TxContext.
/// For each argument, appropriately tracks the creation/freeing of references
fn arguments<'b, 'a: 'b>(
    context: &mut Context,
    args: impl IntoIterator<Item = &'b T::Argument<'a>>,
) -> Result<u64, ExecutionError<'static>> {
    args.into_iter().try_fold(0u64, |held, arg| {
        let is_ref = argument(context, arg)?;
        let incr = if is_ref { 1 } else { 0 };
        Ok(held.saturating_add(incr))
    })
}

/// Returns true iff the argument is a reference type, excluding the TxContext.
/// Counts the creation/freeing of references for the argument and locations/usage
fn argument(
    context: &mut Context,
    sp!(_, (arg, ty)): &T::Argument<'_>,
) -> Result</* is ref */ bool, ExecutionError<'static>> {
    if ty.is_tx_context() != TxContextKind::None {
        // TxContext is excluded from reference safety
        return Ok(false);
    }
    Ok(match arg {
        T::Argument__::Borrow(_, _location) => {
            context.create()?;
            true
        }

        T::Argument__::Freeze(u) => {
            let usage_is_ref = usage(context, u, ty)?;
            assert_invariant!(usage_is_ref, "freeze argument must be a reference type");
            context.free()?;
            context.create()?;
            true
        }
        T::Argument__::Read(u) => {
            if context.protocol_config.fix_ptb_generated_reads() {
                // `ty` is the dereferenced value type, so the location's type must be
                // a reference
                reference_usage(context, u)?;
            } else {
                let usage_is_ref = usage(context, u, ty)?;
                assert_invariant!(usage_is_ref, "read argument must be a reference type");
            }
            context.free()?;
            false
        }
        T::Argument__::Use(u) => usage(context, u, ty)?,
    })
}

/// Returns true iff the argument is a reference type
/// Counts the creation of a new reference for `Copy`
fn usage(
    context: &mut Context,
    u: &T::Usage,
    ty: &T::Type<'_>,
) -> Result<bool, ExecutionError<'static>> {
    if !ty.is_reference() {
        return Ok(false);
    }
    reference_usage(context, u)?;
    Ok(true)
}

/// Tracks the usage of a reference location.
/// Counts the creation of a new reference for `Copy`
fn reference_usage(context: &mut Context, u: &T::Usage) -> Result<(), ExecutionError<'static>> {
    match u {
        T::Usage::Move(_) => Ok(()),
        T::Usage::Copy { .. } => context.create(),
    }
}

impl Context<'_> {
    fn create(&mut self) -> Result<(), ExecutionError<'static>> {
        self.create_n(1)
    }

    fn create_n(&mut self, n: u64) -> Result<(), ExecutionError<'static>> {
        self.live = self.live.saturating_add(n);
        let max_live = self.protocol_config.max_ptb_live_references();
        if self.live > max_live {
            return Err(ExecutionError::new_with_source(
                // TODO introduce an ExecutionErrorKind for limits
                ExecutionErrorKind::InsufficientGas,
                format!(
                    "Command has {} live references, exceeding the maximum of {max_live}",
                    self.live
                ),
            ));
        }
        Ok(())
    }

    /// Creates the `n` references returned by a command, checking both the per-command limit and
    /// the transaction wide total. The total bounds the length of any chain of returned
    /// references, which other analyses are sensitive to beyond the number live at once.
    fn returned_n(&mut self, n: u64) -> Result<(), ExecutionError<'static>> {
        let max_per_command = self.protocol_config.max_ptb_returned_references();
        if n > max_per_command {
            return Err(ExecutionError::new_with_source(
                // TODO introduce an ExecutionErrorKind for limits
                ExecutionErrorKind::InsufficientGas,
                format!(
                    "Command returns {n} references, exceeding the maximum of {max_per_command}"
                ),
            ));
        }
        self.total_returned = self.total_returned.saturating_add(n);
        let max_total = self.protocol_config.max_ptb_total_returned_references();
        if self.total_returned > max_total {
            return Err(ExecutionError::new_with_source(
                // TODO introduce an ExecutionErrorKind for limits
                ExecutionErrorKind::InsufficientGas,
                format!(
                    "Transaction returns {} references, exceeding the maximum of {max_total}",
                    self.total_returned
                ),
            ));
        }
        self.create_n(n)
    }

    fn free(&mut self) -> Result<(), ExecutionError<'static>> {
        self.free_n(1)
    }

    fn free_n(&mut self, n: u64) -> Result<(), ExecutionError<'static>> {
        let Some(rem) = self.live.checked_sub(n) else {
            invariant_violation!("freeing {n} reference(s) when {} are live", self.live)
        };
        self.live = rem;
        Ok(())
    }
}
