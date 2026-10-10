// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    sp,
    static_programmable_transactions::{
        env::Env,
        loading::ast as L,
        typing::{
            ast as T,
            translate::{balance_inner_type, coin_inner_type, withdrawal_inner_type},
            verify::input_arguments,
        },
    },
};
use containers::{Bump, Vec};
use exec_types::{assert_invariant, error::ExecutionError, make_invariant_violation};
use sui_types::{
    allowance::RESOLVED_ALLOWANCE_WITHDRAWAL_STRUCT, coin::RESOLVED_COIN_STRUCT,
    funds_accumulator::RESOLVED_WITHDRAWAL_STRUCT,
};

struct Context<'txn, 'a> {
    objects: Vec<'a, &'txn T::Type<'a>>,
    withdrawals: Vec<'a, &'txn T::Type<'a>>,
    pure: Vec<'a, &'txn T::Type<'a>>,
    receiving: Vec<'a, &'txn T::Type<'a>>,
    result_types: Vec<'a, &'txn [T::Type<'a>]>,
}

impl<'txn, 'a> Context<'txn, 'a> {
    fn new(bump: &'a Bump, txn: &'txn T::Transaction<'a>) -> Self {
        Self {
            objects: collect_in(bump, txn.objects.iter().map(|o| &o.ty)),
            withdrawals: collect_in(bump, txn.withdrawals.iter().map(|w| &w.ty)),
            pure: collect_in(bump, txn.pure.iter().map(|p| &p.ty)),
            receiving: collect_in(bump, txn.receiving.iter().map(|r| &r.ty)),
            result_types: collect_in(
                bump,
                txn.commands
                    .iter()
                    .map(|sp!(_, c)| c.result_type.as_slice()),
            ),
        }
    }
}

fn collect_in<'a, I: ExactSizeIterator>(bump: &'a Bump, iter: I) -> Vec<'a, I::Item> {
    let mut v = Vec::with_capacity_in(iter.len(), bump);
    v.extend(iter);
    v
}

/// The reference's `?` converts its `ExecutionError` into `anyhow::Error` directly; that needs
/// `'static`, so here the error is carried by its message, which is all `verify` keeps.
fn to_anyhow(e: ExecutionError<'_>) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// - All dropped result values have the `drop` ability
pub fn verify<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    txn: &T::Transaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    verify_::<Mode>(env, txn).map_err(|e| make_invariant_violation!("{}. Transaction {:?}", e, txn))
}

fn verify_<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    txn: &T::Transaction<'a>,
) -> anyhow::Result<()> {
    let context = Context::new(env.bump, txn);
    let T::Transaction {
        gas_payment: _,
        bytes: _,
        objects,
        withdrawals,
        pure,
        receiving,
        withdrawal_compatibility_conversions,
        original_command_len: _,
        commands,
        unified_linkage: _,
    } = txn;
    for obj in objects {
        object_input(obj)?;
    }
    for w in withdrawals {
        withdrawal_input(w)?;
    }
    for p in pure {
        pure_input::<Mode>(env.bump, p)?;
    }
    for r in receiving {
        receiving_input(r)?;
    }
    let mut prev_index = 0;
    for c in commands {
        debug_assert!(
            prev_index <= c.idx,
            "command indices should be monotonically increasing"
        );
        prev_index = prev_index.max(c.idx);
        command::<Mode>(env, &context, c)?;
    }
    for (withdrawal, conversion) in withdrawal_compatibility_conversions {
        withdrawal_compatibility_conversion(env, &context, *withdrawal, conversion)?;
    }
    Ok(())
}

fn object_input(obj: &T::ObjectInput) -> anyhow::Result<()> {
    anyhow::ensure!(obj.ty.abilities().has_key(), "object type must have key");
    Ok(())
}

fn withdrawal_input(w: &T::WithdrawalInput) -> anyhow::Result<()> {
    let ty = &w.ty;
    anyhow::ensure!(ty.abilities().has_drop(), "withdrawal type must have drop");
    let T::Type::Datatype(dt) = ty else {
        anyhow::bail!("withdrawal input must be a datatype, got {ty:?}");
    };
    anyhow::ensure!(
        dt.type_arguments.len() == 1,
        "withdrawal input must have exactly one type argument, got {}",
        dt.type_arguments.len()
    );
    // The source and the type must agree on the withdrawal's kind.
    match &w.source {
        T::WithdrawalSource::Direct { .. } => {
            anyhow::ensure!(
                dt.is_resolved(RESOLVED_WITHDRAWAL_STRUCT),
                "direct withdrawal input must be sui::funds_accumulator::Withdrawal, got {:?}",
                dt.qualified_ident()
            );
        }
        T::WithdrawalSource::Allowance { .. } => {
            anyhow::ensure!(
                dt.is_resolved(RESOLVED_ALLOWANCE_WITHDRAWAL_STRUCT),
                "allowance withdrawal input must be sui::allowance::AllowanceWithdrawal, got {:?}",
                dt.qualified_ident()
            );
        }
    }
    Ok(())
}

fn pure_input<Mode: ExecutionMode>(bump: &Bump, p: &T::PureInput) -> anyhow::Result<()> {
    if !Mode::allow_arbitrary_values() {
        anyhow::ensure!(
            input_arguments::is_valid_pure_type(bump, &p.ty).map_err(to_anyhow)?,
            "pure type must be valid"
        );
    }
    Ok(())
}

fn receiving_input(r: &T::ReceivingInput) -> anyhow::Result<()> {
    anyhow::ensure!(
        input_arguments::is_valid_receiving(&r.ty),
        "receiving type must be valid"
    );
    Ok(())
}

fn command<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &Context<'_, 'a>,
    sp!(_, c): &T::Command<'a>,
) -> anyhow::Result<()> {
    let result_tys = &c.result_type;
    match &c.command {
        T::Command__::MoveCall(move_call) => {
            let T::MoveCall {
                function,
                arguments,
            } = &**move_call;
            let L::LoadedFunction { signature, .. } = function;
            let L::LoadedFunctionInstantiation {
                parameters,
                return_,
            } = signature;
            anyhow::ensure!(
                arguments.len() == parameters.len(),
                "arity mismatch. Expected {}, got {}",
                parameters.len(),
                arguments.len()
            );
            // The reference's `zip_debug_eq`; the lengths are equal by the check above.
            #[allow(clippy::disallowed_methods)]
            for (arg, param) in arguments.iter().zip(parameters) {
                argument(env, context, arg, param)?;
            }
            anyhow::ensure!(
                return_.len() == result_tys.len(),
                "result arity mismatch. Expected {}, got {}",
                return_.len(),
                result_tys.len()
            );
            #[allow(clippy::disallowed_methods)]
            for (actual, expected) in return_.iter().zip(result_tys) {
                anyhow::ensure!(
                    actual == expected,
                    "return type mismatch. Expected {expected:?}, got {actual:?}"
                );
            }
        }
        T::Command__::TransferObjects(objs, recipient) => {
            for obj in objs {
                let ty = &obj.value.1;
                anyhow::ensure!(
                    ty.abilities().has_key(),
                    "transfer object type must have key, got {ty:?}"
                );
                argument(env, context, obj, ty)?;
            }
            argument(env, context, recipient, &T::Type::Address)?;
            anyhow::ensure!(
                result_tys.is_empty(),
                "transfer objects should not return any value, got {result_tys:?}"
            );
        }
        T::Command__::SplitCoins(ty_coin, coin, amounts) => {
            let T::Type::Datatype(dt) = ty_coin else {
                anyhow::bail!("split coins should have a coin type, got {ty_coin:?}");
            };
            let resolved = dt.qualified_ident();
            anyhow::ensure!(
                dt.is_resolved(RESOLVED_COIN_STRUCT),
                "split coins should have a coin type, got {resolved:?}"
            );
            argument(env, context, coin, &T::Type::Reference(true, ty_coin))?;
            for amount in amounts {
                argument(env, context, amount, &T::Type::U64)?;
            }
            anyhow::ensure!(
                amounts.len() == result_tys.len(),
                "split coins should return as many values as amounts, expected {} got {}",
                amounts.len(),
                result_tys.len()
            );
            anyhow::ensure!(
                result_tys.iter().all(|t| t == ty_coin),
                "split coins should return coin<{ty_coin:?}>, got {result_tys:?}"
            );
        }
        T::Command__::MergeCoins(ty_coin, target, coins) => {
            let T::Type::Datatype(dt) = ty_coin else {
                anyhow::bail!("split coins should have a coin type, got {ty_coin:?}");
            };
            let resolved = dt.qualified_ident();
            anyhow::ensure!(
                dt.is_resolved(RESOLVED_COIN_STRUCT),
                "split coins should have a coin type, got {resolved:?}"
            );
            argument(env, context, target, &T::Type::Reference(true, ty_coin))?;
            for coin in coins {
                argument(env, context, coin, ty_coin)?;
            }
            anyhow::ensure!(
                result_tys.is_empty(),
                "merge coins should not return any value, got {result_tys:?}"
            );
        }
        T::Command__::MakeMoveVec(t, args) => {
            for arg in args {
                argument(env, context, arg, t)?;
            }
            anyhow::ensure!(
                result_tys.len() == 1,
                "make move vec should return exactly one vector"
            );
            let T::Type::Vector(inner) = result_tys.first().unwrap() else {
                anyhow::bail!("make move vec should return a vector type, got {result_tys:?}");
            };
            anyhow::ensure!(
                t == &inner.element_type,
                "make move vec should return vector<{t:?}>, got {result_tys:?}"
            );
        }
        T::Command__::Publish(_, _, _) => {
            if Mode::packages_are_predefined() {
                anyhow::ensure!(
                    result_tys.is_empty(),
                    "publish should not return upgrade cap for predefined packages"
                );
            } else {
                anyhow::ensure!(
                    result_tys.len() == 1,
                    "publish should return exactly one upgrade cap"
                );
                let cap = &env.upgrade_cap_type().map_err(to_anyhow)?;
                anyhow::ensure!(
                    cap == result_tys.first().unwrap(),
                    "publish should return {cap:?}, got {result_tys:?}",
                );
            }
        }
        T::Command__::Upgrade(_, _, _, arg, _) => {
            argument(
                env,
                context,
                arg,
                &env.upgrade_ticket_type().map_err(to_anyhow)?,
            )?;
            let receipt = &env.upgrade_receipt_type().map_err(to_anyhow)?;
            anyhow::ensure!(
                result_tys.len() == 1,
                "upgrade should return exactly one receipt"
            );
            anyhow::ensure!(
                receipt == result_tys.first().unwrap(),
                "upgrade should return {receipt:?}, got {result_tys:?}"
            );
        }
    }
    assert_invariant!(
        c.drop_values.len() == result_tys.len(),
        "drop values should match result types, expected {} got {}",
        c.drop_values.len(),
        result_tys.len()
    );
    // The reference's `zip_debug_eq`; the lengths are equal by the check above.
    #[allow(clippy::disallowed_methods)]
    for (drop_value, result_ty) in c.drop_values.iter().copied().zip(result_tys) {
        // drop value ==> `ty: drop`
        assert_invariant!(
            !drop_value || result_ty.abilities().has_drop(),
            "result was marked for drop but does not have the `drop` ability"
        );
    }
    Ok(())
}

fn argument<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &Context<'_, 'a>,
    sp!(_, (arg__, ty)): &T::Argument<'a>,
    param: &T::Type<'_>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        ty == param,
        "argument type mismatch. Expected {param:?}, got {ty:?}"
    );
    let (actual, expected) = match arg__ {
        T::Argument__::Use(u) => (usage(env, context, u)?, param),
        T::Argument__::Read(u) => {
            let actual = match usage(env, context, u)? {
                T::Type::Reference(_, inner) => *inner,
                _ => {
                    anyhow::bail!("should never ReadRef a non-reference type, got {ty:?}");
                }
            };
            (actual, param)
        }
        T::Argument__::Freeze(u) => {
            let actual = match usage(env, context, u)? {
                T::Type::Reference(true, inner) => T::Type::Reference(false, inner),
                T::Type::Reference(false, _) => {
                    anyhow::bail!("should never FreezeRef an immutable reference")
                }
                ty => {
                    anyhow::bail!("should never Freeze a non-reference type, got {ty:?}");
                }
            };
            (actual, param)
        }
        T::Argument__::Borrow(is_mut, l) => {
            let T::Type::Reference(param_mut, expected) = param else {
                anyhow::bail!("expected a reference type for borrowed location, got {param:?}");
            };
            anyhow::ensure!(
                *param_mut == *is_mut,
                "borrowed location mutability mismatch. Expected {param_mut}, got {is_mut}"
            );
            let actual = location(env, context, *l)?;
            (actual, *expected)
        }
    };
    // check actual == expected
    anyhow::ensure!(
        &actual == expected,
        "argument type mismatch. Expected {expected:?}, got {actual:?}"
    );
    // check copy usage
    match arg__ {
        T::Argument__::Use(T::Usage::Copy { .. }) | T::Argument__::Read(_) => {
            anyhow::ensure!(
                param.abilities().has_copy(),
                "expected type does not have copy, {expected:?}"
            );
        }
        T::Argument__::Use(T::Usage::Move(_))
        | T::Argument__::Freeze(_)
        | T::Argument__::Borrow(_, _) => (),
    }
    Ok(())
}

fn usage<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &Context<'_, 'a>,
    u: &T::Usage,
) -> anyhow::Result<T::Type<'a>> {
    match u {
        T::Usage::Move(l)
        | T::Usage::Copy {
            location: l,
            borrowed: _,
        } => location(env, context, *l),
    }
}

fn location<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &Context<'_, 'a>,
    l: T::Location,
) -> anyhow::Result<T::Type<'a>> {
    Ok(match l {
        T::Location::TxContext => env.tx_context_type().map_err(to_anyhow)?,
        T::Location::GasCoin => env.gas_coin_type().map_err(to_anyhow)?,
        T::Location::ObjectInput(i) => *context
            .objects
            .get(i as usize)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("object input {i} out of bounds"))?,
        T::Location::WithdrawalInput(i) => *context
            .withdrawals
            .get(i as usize)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("withdrawal input {i} out of bounds"))?,
        T::Location::PureInput(i) => *context
            .pure
            .get(i as usize)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("pure input {i} out of bounds"))?,
        T::Location::ReceivingInput(i) => *context
            .receiving
            .get(i as usize)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("receiving input {i} out of bounds"))?,
        T::Location::Result(i, j) => *context
            .result_types
            .get(i as usize)
            .and_then(|v| v.get(j as usize))
            .ok_or_else(|| anyhow::anyhow!("result ({i}, {j}) out of bounds",))?,
    })
}

fn withdrawal_compatibility_conversion<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &Context<'_, 'a>,
    withdrawal_location: T::Location,
    conv: &T::WithdrawalCompatibilityConversion,
) -> anyhow::Result<()> {
    let T::WithdrawalCompatibilityConversion {
        owner,
        conversion_result,
    } = conv;
    // checker owner is a pure input of type address
    anyhow::ensure!(
        matches!(owner, T::Location::PureInput(_)),
        "withdrawal compatibility conversion owner should be a pure input"
    );
    anyhow::ensure!(
        location(env, context, *owner)? == T::Type::Address,
        "withdrawal compatibility conversion owner type should be address"
    );
    // check the conversion result type is coin
    let conversion_location = T::Location::Result(*conversion_result, 0);
    let conversion_ty = location(env, context, conversion_location)?;
    let Some(coin_inner) = coin_inner_type(&conversion_ty) else {
        anyhow::bail!("conversion result should be a coin type");
    };
    // check the withdrawal location is a withdrawal input of Withdarawal<Balance<coin_inner>>
    anyhow::ensure!(
        matches!(withdrawal_location, T::Location::WithdrawalInput(_)),
        "withdrawal should be a withdrawal input"
    );
    let withdrawal_ty = location(env, context, withdrawal_location)?;
    let Some(withdrawal_inner_ty) = withdrawal_inner_type(&withdrawal_ty) else {
        anyhow::bail!("withdrawal input should be a withdrawal type");
    };
    let Some(withdrawal_balance_inner) = balance_inner_type(withdrawal_inner_ty) else {
        anyhow::bail!("withdrawal inner type should be a balance type");
    };
    anyhow::ensure!(
        withdrawal_balance_inner == coin_inner,
        "withdrawal balance inner type should match conversion coin inner type"
    );
    Ok(())
}
