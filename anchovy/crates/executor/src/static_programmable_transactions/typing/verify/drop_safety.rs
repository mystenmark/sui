// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    static_programmable_transactions::{env::Env, typing::ast as T},
};
use exec_types::error::ExecutionError;

/// Refines usage of values so that the last `Copy` of a value is a `Move` if it is not borrowed
/// After, it verifies the following
/// - No results without `drop` are unused (all unused non-input values have `drop`)
pub fn refine_and_verify<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    ast: &mut T::Transaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    refine::transaction(env, ast)?;
    verify::transaction::<Mode>(env, ast)?;
    Ok(())
}

mod refine {
    use crate::execution_mode::ExecutionMode;
    use sui_types::coin::{COIN_MODULE_NAME, SEND_FUNDS_FUNC_NAME};

    use crate::{
        sp,
        static_programmable_transactions::{
            env::Env,
            spanned::sp,
            typing::{
                ast::{self as T, Type},
                translate::coin_inner_type,
            },
        },
    };
    use containers::{BTreeSet, Bump, Vec};
    use exec_types::error::ExecutionError;
    use exec_types::{assert_invariant, checked_as, invariant_violation};

    struct Context<'a> {
        // All locations that were used
        used: BTreeSet<'a, T::Location>,
        // All locations that were used via a Move. This is a subset of `used`, but `used` does
        // not track how each value was used in each instance. For simplicity, this is kept as a
        // separate set.
        moved: BTreeSet<'a, T::Location>,
    }

    impl<'a> Context<'a> {
        fn new(bump: &'a Bump) -> Self {
            Self {
                used: BTreeSet::new_in(bump),
                moved: BTreeSet::new_in(bump),
            }
        }
    }

    /// After memory safety, we can switch the last usage of a `Copy` to a `Move` if it is not
    /// borrowed at the time of the last usage.
    pub fn transaction<'a, Mode: ExecutionMode>(
        env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
        ast: &mut T::Transaction<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        let mut context = Context::new(env.bump);
        for c in ast.commands.iter_mut().rev() {
            command(&mut context, c);
        }
        return_unused_withdrawal_conversions(env, ast, &context.moved)
    }

    fn command(context: &mut Context<'_>, sp!(_, c): &mut T::Command<'_>) {
        match &mut c.command {
            T::Command__::MoveCall(mc) => arguments(context, &mut mc.arguments),
            T::Command__::TransferObjects(objects, recipient) => {
                argument(context, recipient);
                arguments(context, objects);
            }
            T::Command__::SplitCoins(_, coin, amounts) => {
                arguments(context, amounts);
                argument(context, coin);
            }
            T::Command__::MergeCoins(_, target, coins) => {
                arguments(context, coins);
                argument(context, target);
            }
            T::Command__::MakeMoveVec(_, xs) => arguments(context, xs),
            T::Command__::Publish(_, _, _) => (),
            T::Command__::Upgrade(_, _, _, x, _) => argument(context, x),
        }
    }

    fn arguments(context: &mut Context<'_>, args: &mut [T::Argument<'_>]) {
        for arg in args.iter_mut().rev() {
            argument(context, arg)
        }
    }

    fn argument(context: &mut Context<'_>, arg: &mut T::Argument<'_>) {
        let usage = match &mut arg.value.0 {
            T::Argument__::Use(u) | T::Argument__::Read(u) | T::Argument__::Freeze(u) => u,
            T::Argument__::Borrow(_, loc) => {
                // mark location as used
                context.used.insert(*loc);
                return;
            }
        };
        match &usage {
            T::Usage::Move(loc) => {
                // mark location as used
                context.used.insert(*loc);
                context.moved.insert(*loc);
            }
            T::Usage::Copy { location, borrowed } => {
                // we are at the last usage of a reference result if it was not yet added to the set
                let location = *location;
                let last_usage = context.used.insert(location);
                if last_usage && !borrowed.get().unwrap() {
                    // if it was the last usage, we need to change the Copy to a Move
                    *usage = T::Usage::Move(location);
                    context.moved.insert(location);
                }
            }
        }
    }

    /// For any withdrawal conversion where the value was not moved, send it back to the original
    /// owner
    fn return_unused_withdrawal_conversions<'a, Mode: ExecutionMode>(
        env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
        ast: &mut T::Transaction<'a>,
        moved_locations: &BTreeSet<'_, T::Location>,
    ) -> Result<(), ExecutionError<'a>> {
        // withdrawal conversions not empty ==> accumulators enabled
        assert_invariant!(
            ast.withdrawal_compatibility_conversions.is_empty()
                || env.protocol_config.enable_accumulators(),
            "Withdrawal conversions should be empty if accumulators are not enabled"
        );
        for conversion_info in
            ast.withdrawal_compatibility_conversions
                .values()
                .filter(|conversion| {
                    // The conversion result is always a single return value, as such we can specify
                    // a secondary index of `0` without worrying about other results.
                    let conversion_location = T::Location::Result(conversion.conversion_result, 0);
                    !moved_locations.contains(&conversion_location)
                })
        {
            let Some(cur_command) = ast.commands.len().checked_sub(1) else {
                invariant_violation!("cannot be zero commands with a conversion")
            };
            let cur_command = checked_as!(cur_command, u16)?;
            let T::WithdrawalCompatibilityConversion {
                owner,
                conversion_result,
            } = *conversion_info;
            let Some(conversion_command) = ast.commands.get(conversion_result as usize) else {
                invariant_violation!("conversion result should be a valid command index")
            };
            assert_invariant!(
                conversion_command.value.result_type.len() == 1,
                "conversion should have one result"
            );
            let T::Location::PureInput(owner_pure_idx) = owner else {
                invariant_violation!("owner should be a pure input")
            };
            assert_invariant!(
                ast.pure.len() > owner_pure_idx as usize,
                "owner pure input index out of bounds"
            );
            assert_invariant!(
                ast.pure.get(owner_pure_idx as usize).unwrap().ty == T::Type::Address,
                "owner pure input should be an address"
            );
            let Some(conversion_ty) = conversion_command.value.result_type.first().copied() else {
                invariant_violation!("conversion should have a result type")
            };
            let Some(inner_ty) = coin_inner_type(&conversion_ty).copied() else {
                invariant_violation!("conversion result should be a coin type")
            };
            let move_result_ = T::Argument__::new_move(T::Location::Result(conversion_result, 0));
            let move_result = sp(cur_command, (move_result_, conversion_ty));
            let owner_ty = Type::Address;
            let owner_arg_ = T::Argument__::new_move(owner);
            let owner_arg = sp(cur_command, (owner_arg_, owner_ty));
            let mut type_arguments = Vec::with_capacity_in(1, env.bump);
            type_arguments.push(inner_ty);
            let mut arguments = Vec::with_capacity_in(2, env.bump);
            arguments.push(move_result);
            arguments.push(owner_arg);
            let return_command__ = T::Command__::MoveCall(containers::Box::new_in(
                T::MoveCall {
                    function: env.load_framework_function(
                        COIN_MODULE_NAME,
                        SEND_FUNDS_FUNC_NAME,
                        type_arguments,
                        ast.unified_linkage.as_ref(),
                    )?,
                    arguments,
                },
                env.bump,
            ));
            let return_command = sp(
                cur_command,
                T::Command_ {
                    command: return_command__,
                    result_type: Vec::new_in(env.bump),
                    drop_values: Vec::new_in(env.bump),
                    incurs_post_execution_checks: false,
                },
            );
            ast.commands.push(return_command);
        }
        Ok(())
    }
}

mod verify {
    use crate::{
        execution_mode::ExecutionMode,
        sp,
        static_programmable_transactions::{
            env::Env,
            typing::ast::{self as T, Type},
        },
    };
    use containers::{Bump, Vec};
    use exec_types::error::{ExecutionError, SafeIndex};
    use exec_types::{assert_invariant, checked_as, invariant_violation};
    use messages::execution_status::ExecutionErrorKind;

    #[must_use]
    struct Value;

    struct Context<'a> {
        tx_context: Option<Value>,
        gas_coin: Option<Value>,
        objects: Vec<'a, Option<Value>>,
        withdrawals: Vec<'a, Option<Value>>,
        pure: Vec<'a, Option<Value>>,
        receiving: Vec<'a, Option<Value>>,
        results: Vec<'a, Vec<'a, Option<Value>>>,
    }

    /// `n` values of `Some(Value)`, in the arena.
    fn values(bump: &Bump, n: usize) -> Vec<'_, Option<Value>> {
        let mut values = Vec::with_capacity_in(n, bump);
        values.extend((0..n).map(|_| Some(Value)));
        values
    }

    impl<'a> Context<'a> {
        fn new<Mode: ExecutionMode>(
            env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
            ast: &T::Transaction<'a>,
        ) -> Self {
            let bump = env.bump;
            let objects = values(bump, ast.objects.len());
            let withdrawals = values(bump, ast.withdrawals.len());
            let pure = values(bump, ast.pure.len());
            let receiving = values(bump, ast.receiving.len());
            let gas_coin = if ast.gas_payment.is_none() {
                None
            } else {
                Some(Value)
            };
            Self {
                tx_context: Some(Value),
                gas_coin,
                objects,
                withdrawals,
                pure,
                receiving,
                results: Vec::with_capacity_in(ast.commands.len(), bump),
            }
        }

        fn location(&mut self, l: T::Location) -> Result<&mut Option<Value>, ExecutionError<'a>> {
            Ok(match l {
                T::Location::TxContext => &mut self.tx_context,
                T::Location::GasCoin => &mut self.gas_coin,
                T::Location::ObjectInput(i) => self.objects.safe_get_mut(i as usize)?,
                T::Location::WithdrawalInput(i) => self.withdrawals.safe_get_mut(i as usize)?,
                T::Location::PureInput(i) => self.pure.safe_get_mut(i as usize)?,
                T::Location::ReceivingInput(i) => self.receiving.safe_get_mut(i as usize)?,
                T::Location::Result(i, j) => self
                    .results
                    .safe_get_mut(i as usize)?
                    .safe_get_mut(j as usize)?,
            })
        }
    }

    /// Checks the following
    /// - All unused result values have `drop`
    // `zip` stands in for the reference's `zip_debug_eq`: the lengths are asserted first.
    #[allow(clippy::disallowed_methods)]
    pub fn transaction<'a, Mode: ExecutionMode>(
        env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
        ast: &T::Transaction<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        let bump = env.bump;
        let mut context = Context::new(env, ast);
        let commands = &ast.commands;
        for c in commands {
            let result =
                command(bump, &mut context, c).map_err(|e| e.with_command_index(c.idx as usize))?;
            assert_invariant!(
                result.len() == c.value.result_type.len(),
                "result length mismatch"
            );
            // drop unused result values
            assert_invariant!(
                result.len() == c.value.drop_values.len(),
                "drop values length mismatch"
            );
            let mut result_values = Vec::with_capacity_in(result.len(), bump);
            result_values.extend(
                result
                    .into_iter()
                    .zip(c.value.drop_values.iter().copied())
                    .map(|(v, drop)| {
                        if !drop {
                            Some(v)
                        } else {
                            consume_value(v);
                            None
                        }
                    }),
            );
            context.results.push(result_values);
        }

        let Context {
            tx_context,
            gas_coin,
            objects,
            withdrawals,
            pure,
            receiving,
            results,
        } = context;
        consume_value_opt(gas_coin);
        // TODO do we want to check inputs in the dev inspect case?
        consume_value_opts(objects);
        consume_value_opts(withdrawals);
        consume_value_opts(pure);
        consume_value_opts(receiving);
        assert_invariant!(results.len() == commands.len(), "result length mismatch");
        for (i, (result, c)) in results.into_iter().zip(&ast.commands).enumerate() {
            let tys = &c.value.result_type;
            assert_invariant!(result.len() == tys.len(), "result length mismatch");
            for (j, (vopt, ty)) in result.into_iter().zip(tys).enumerate() {
                drop_value_opt::<Mode>((i, j), vopt, ty)?;
            }
        }
        assert_invariant!(tx_context.is_some(), "tx_context should never be moved");
        Ok(())
    }

    fn command<'a>(
        bump: &'a Bump,
        context: &mut Context<'a>,
        sp!(_, c): &T::Command<'a>,
    ) -> Result<Vec<'a, Value>, ExecutionError<'a>> {
        let result_tys = &c.result_type;
        Ok(match &c.command {
            T::Command__::MoveCall(mc) => {
                let T::MoveCall {
                    function,
                    arguments: args,
                } = &**mc;
                let return_ = &function.signature.return_;
                let arg_values = arguments(bump, context, args)?;
                consume_values(arg_values);
                values_of_len(bump, return_.len())
            }
            T::Command__::TransferObjects(objects, recipient) => {
                let object_values = arguments(bump, context, objects)?;
                let recipient_value = argument(context, recipient)?;
                consume_values(object_values);
                consume_value(recipient_value);
                Vec::new_in(bump)
            }
            T::Command__::SplitCoins(_, coin, amounts) => {
                let coin_value = argument(context, coin)?;
                let amount_values = arguments(bump, context, amounts)?;
                consume_values(amount_values);
                consume_value(coin_value);
                values_of_len(bump, amounts.len())
            }
            T::Command__::MergeCoins(_, target, coins) => {
                let target_value = argument(context, target)?;
                let coin_values = arguments(bump, context, coins)?;
                consume_values(coin_values);
                consume_value(target_value);
                Vec::new_in(bump)
            }
            T::Command__::MakeMoveVec(_, xs) => {
                let vs = arguments(bump, context, xs)?;
                consume_values(vs);
                values_of_len(bump, 1)
            }
            T::Command__::Publish(_, _, _) => values_of_len(bump, result_tys.len()),
            T::Command__::Upgrade(_, _, _, x, _) => {
                let v = argument(context, x)?;
                consume_value(v);
                values_of_len(bump, 1)
            }
        })
    }

    /// `n` values, in the arena.
    fn values_of_len(bump: &Bump, n: usize) -> Vec<'_, Value> {
        let mut values = Vec::with_capacity_in(n, bump);
        values.extend((0..n).map(|_| Value));
        values
    }

    fn consume_values(_: Vec<'_, Value>) {}

    fn consume_value(_: Value) {}

    fn consume_value_opts(_: Vec<'_, Option<Value>>) {}

    fn consume_value_opt(_: Option<Value>) {}

    fn drop_value_opt<'a, Mode: ExecutionMode>(
        idx: (usize, usize),
        value: Option<Value>,
        ty: &Type<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        match value {
            Some(v) => drop_value::<Mode>(idx, v, ty),
            None => Ok(()),
        }
    }

    fn drop_value<'a, Mode: ExecutionMode>(
        (i, j): (usize, usize),
        value: Value,
        ty: &Type<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        let abilities = ty.abilities();
        if !abilities.has_drop() && !Mode::allow_arbitrary_values() {
            let msg = if abilities.has_copy() {
                "The value has copy, but not drop. \
                Its last usage must be by-value so it can be taken."
            } else {
                "Unused value without drop"
            };
            return Err(ExecutionError::new_with_source(
                ExecutionErrorKind::UnusedValueWithoutDrop {
                    result_idx: checked_as!(i, u16)?,
                    secondary_idx: checked_as!(j, u16)?,
                },
                msg,
            ));
        }
        consume_value(value);
        Ok(())
    }

    fn arguments<'a>(
        bump: &'a Bump,
        context: &mut Context<'a>,
        xs: &[T::Argument<'a>],
    ) -> Result<Vec<'a, Value>, ExecutionError<'a>> {
        let mut values = Vec::with_capacity_in(xs.len(), bump);
        for x in xs {
            values.push(argument(context, x)?);
        }
        Ok(values)
    }

    fn argument<'a>(
        context: &mut Context<'a>,
        sp!(_, x): &T::Argument<'a>,
    ) -> Result<Value, ExecutionError<'a>> {
        match &x.0 {
            T::Argument__::Use(T::Usage::Move(location)) => move_value(context, *location),
            T::Argument__::Use(T::Usage::Copy { location, .. }) => copy_value(context, *location),
            T::Argument__::Borrow(_, location) => borrow_location(context, *location),
            T::Argument__::Read(usage) => read_ref(context, usage),
            T::Argument__::Freeze(usage) => freeze_ref(context, usage),
        }
    }

    fn move_value<'a>(
        context: &mut Context<'a>,
        l: T::Location,
    ) -> Result<Value, ExecutionError<'a>> {
        let Some(value) = context.location(l)?.take() else {
            invariant_violation!("memory safety should have failed")
        };
        Ok(value)
    }

    fn copy_value<'a>(
        context: &mut Context<'a>,
        l: T::Location,
    ) -> Result<Value, ExecutionError<'a>> {
        assert_invariant!(
            context.location(l)?.is_some(),
            "memory safety should have failed"
        );
        Ok(Value)
    }

    fn borrow_location<'a>(
        context: &mut Context<'a>,
        l: T::Location,
    ) -> Result<Value, ExecutionError<'a>> {
        assert_invariant!(
            context.location(l)?.is_some(),
            "memory safety should have failed"
        );
        Ok(Value)
    }

    fn read_ref<'a>(context: &mut Context<'a>, u: &T::Usage) -> Result<Value, ExecutionError<'a>> {
        let value = match u {
            T::Usage::Move(l) => move_value(context, *l)?,
            T::Usage::Copy { location, .. } => copy_value(context, *location)?,
        };
        consume_value(value);
        Ok(Value)
    }

    fn freeze_ref<'a>(
        context: &mut Context<'a>,
        u: &T::Usage,
    ) -> Result<Value, ExecutionError<'a>> {
        let value = match u {
            T::Usage::Move(l) => move_value(context, *l)?,
            T::Usage::Copy { location, .. } => copy_value(context, *location)?,
        };
        consume_value(value);
        Ok(Value)
    }
}
