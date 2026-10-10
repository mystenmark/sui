// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    sp,
    static_programmable_transactions::execution::context::{
        PrimitiveArgumentLayout, bcs_argument_validate,
    },
    static_programmable_transactions::{
        env::Env,
        loading::ast::Type,
        typing::ast::{self as T, BytesConstraint},
    },
};
use containers::{Bump, IndexSet, Vec};
use exec_types::error::{ExecutionError, SafeIndex, command_argument_error};
use exec_types::{assert_invariant, invariant_violation};
use messages::execution_status::{CommandArgumentError, ExecutionErrorKind};
use sui_types::{
    SUI_FRAMEWORK_ADDRESS,
    base_types::{RESOLVED_ASCII_STR, RESOLVED_STD_OPTION, RESOLVED_UTF8_STR},
    coin::{COIN_MODULE_NAME, SEND_FUNDS_FUNC_NAME},
    id::RESOLVED_SUI_ID,
    transfer::RESOLVED_RECEIVING_STRUCT,
};

struct ObjectUsage {
    allow_by_value: bool,
    allow_by_mut_ref: bool,
}

struct Context<'a> {
    objects: Vec<'a, ObjectUsage>,
}

impl<'a> Context<'a> {
    fn new(bump: &'a Bump, txn: &T::Transaction<'a>) -> Self {
        let mut objects = Vec::with_capacity_in(txn.objects.len(), bump);
        objects.extend(txn.objects.iter().map(|object_input| {
            let allow_by_value = object_input.arg.refined_permissions.can_use_mutably();
            let allow_by_mut_ref = object_input.arg.refined_permissions.can_use_mutably();
            ObjectUsage {
                allow_by_value,
                allow_by_mut_ref,
            }
        }));
        Self { objects }
    }
}

/// Verifies two properties for input objects:
/// 1. That the `Pure` inputs can be serialized to the type inferred and that the type is
///    permissible
///    - Can be relaxed under certain execution modes
/// 2. That any `Object` arguments are used validly. This means mutable references are taken only
///    on mutable objects. And that the gas coin is only taken by value in transfer objects or with
///    `sui::coin::send_funds`.
pub fn verify<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    txn: &T::Transaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    let T::Transaction {
        gas_payment: _,
        bytes,
        objects: _,
        withdrawals: _,
        pure,
        receiving,
        withdrawal_compatibility_conversions: _,
        original_command_len: _,
        commands,
        unified_linkage: _,
    } = txn;
    for pure in pure {
        check_pure_input::<Mode>(env.bump, bytes, pure)?;
    }
    for receiving in receiving {
        check_receiving_input(receiving)?;
    }
    let context = &mut Context::new(env.bump, txn);
    for c in commands {
        command(env, context, c).map_err(|e| e.with_command_index(c.idx as usize))?;
    }
    Ok(())
}

//**************************************************************************************************
// Pure bytes
//**************************************************************************************************

fn check_pure_input<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    bytes: &IndexSet<'a, &'a [u8]>,
    pure: &T::PureInput<'a>,
) -> Result<(), ExecutionError<'a>> {
    let T::PureInput {
        original_input_index,
        byte_index,
        ty,
        constraint,
    } = pure;
    let Some(bcs_bytes) = bytes.get_index(*byte_index) else {
        invariant_violation!(
            "Unbound byte index {} for pure input at index {}",
            byte_index,
            original_input_index.0
        );
    };
    let BytesConstraint { command, argument } = constraint;
    check_pure_bytes::<Mode>(bump, *argument, bcs_bytes, ty)
        .map_err(|e| e.with_command_index(*command as usize))
}

fn check_pure_bytes<'a, Mode: ExecutionMode>(
    bump: &'a Bump,
    command_arg_idx: u16,
    bytes: &[u8],
    constraint: &Type<'a>,
) -> Result<(), ExecutionError<'a>> {
    assert_invariant!(
        !matches!(constraint, Type::Reference(_, _)),
        "references should not be added as a constraint"
    );
    if Mode::allow_arbitrary_values() {
        return Ok(());
    }
    let Some(layout) = primitive_serialization_layout(bump, constraint)? else {
        let msg = format!(
            "Invalid usage of `Pure` argument for a non-primitive argument type at index {command_arg_idx}.",
        );
        return Err(ExecutionError::new_with_source(
            ExecutionErrorKind::command_argument_error(
                CommandArgumentError::InvalidUsageOfPureArg,
                command_arg_idx,
            ),
            msg,
        ));
    };
    bcs_argument_validate(bytes, command_arg_idx, layout)?;
    Ok(())
}

// The reference boxes the nested layouts; here they are in the arena.
fn primitive_serialization_layout<'a>(
    bump: &'a Bump,
    param_ty: &Type<'a>,
) -> Result<Option<PrimitiveArgumentLayout<'a>>, ExecutionError<'a>> {
    Ok(match param_ty {
        Type::Signer => return Ok(None),
        Type::Reference(_, _) => {
            invariant_violation!("references should not be added as a constraint")
        }
        Type::Bool => Some(PrimitiveArgumentLayout::Bool),
        Type::U8 => Some(PrimitiveArgumentLayout::U8),
        Type::U16 => Some(PrimitiveArgumentLayout::U16),
        Type::U32 => Some(PrimitiveArgumentLayout::U32),
        Type::U64 => Some(PrimitiveArgumentLayout::U64),
        Type::U128 => Some(PrimitiveArgumentLayout::U128),
        Type::U256 => Some(PrimitiveArgumentLayout::U256),
        Type::Address => Some(PrimitiveArgumentLayout::Address),

        Type::Vector(v) => {
            let info_opt = primitive_serialization_layout(bump, &v.element_type)?;
            info_opt.map(|layout| PrimitiveArgumentLayout::Vector(containers::alloc(bump, layout)))
        }
        Type::Datatype(dt) => {
            // is option of a string
            if dt.is_resolved(RESOLVED_STD_OPTION) && dt.type_arguments.len() == 1 {
                let info_opt =
                    primitive_serialization_layout(bump, dt.type_arguments.first().unwrap())?;
                info_opt
                    .map(|layout| PrimitiveArgumentLayout::Option(containers::alloc(bump, layout)))
            } else if dt.type_arguments.is_empty() {
                if dt.is_resolved(RESOLVED_SUI_ID) {
                    Some(PrimitiveArgumentLayout::Address)
                } else if dt.is_resolved(RESOLVED_ASCII_STR) {
                    Some(PrimitiveArgumentLayout::Ascii)
                } else if dt.is_resolved(RESOLVED_UTF8_STR) {
                    Some(PrimitiveArgumentLayout::UTF8)
                } else {
                    None
                }
            } else {
                None
            }
        }
    })
}

fn check_receiving_input<'a>(receiving: &T::ReceivingInput<'a>) -> Result<(), ExecutionError<'a>> {
    let T::ReceivingInput {
        original_input_index: _,
        object_ref: _,
        ty,
        constraint,
    } = receiving;
    let BytesConstraint { command, argument } = constraint;
    check_receiving(*argument, ty).map_err(|e| e.with_command_index(*command as usize))
}

fn check_receiving<'a>(
    command_arg_idx: u16,
    constraint: &Type<'a>,
) -> Result<(), ExecutionError<'a>> {
    if is_valid_receiving(constraint) {
        Ok(())
    } else {
        Err(command_argument_error(
            CommandArgumentError::TypeMismatch,
            command_arg_idx as usize,
        ))
    }
}

pub fn is_valid_pure_type<'a>(
    bump: &'a Bump,
    constraint: &Type<'a>,
) -> Result<bool, ExecutionError<'a>> {
    Ok(primitive_serialization_layout(bump, constraint)?.is_some())
}

/// Returns true if a type is a `Receiving<t>` where `t` has `key`
pub fn is_valid_receiving(constraint: &Type<'_>) -> bool {
    let Type::Datatype(dt) = constraint else {
        return false;
    };
    dt.is_resolved(RESOLVED_RECEIVING_STRUCT)
        && dt.type_arguments.len() == 1
        && dt.type_arguments.first().unwrap().abilities().has_key()
}

//**************************************************************************************************
// Object usage
//**************************************************************************************************

fn command<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    context: &mut Context<'a>,
    sp!(_, c): &T::Command<'a>,
) -> Result<(), ExecutionError<'a>> {
    match &c.command {
        T::Command__::MoveCall(mc) => {
            check_obj_usages(context, &mc.arguments)?;
            if !(env.protocol_config.enable_accumulators() && is_coin_send_funds(&mc.function)) {
                // We allow the gas coin to be used with `sui::coin::send_funds`
                check_gas_by_values(&mc.arguments)?;
            }
        }
        T::Command__::TransferObjects(objects, recipient) => {
            check_obj_usages(context, objects)?;
            check_obj_usage(context, recipient)?;
            // gas can be used by value in TransferObjects
        }
        T::Command__::SplitCoins(_, coin, amounts) => {
            check_obj_usage(context, coin)?;
            check_obj_usages(context, amounts)?;
            check_gas_by_value(coin)?;
            check_gas_by_values(amounts)?;
        }
        T::Command__::MergeCoins(_, target, coins) => {
            check_obj_usage(context, target)?;
            check_obj_usages(context, coins)?;
            check_gas_by_value(target)?;
            check_gas_by_values(coins)?;
        }
        T::Command__::MakeMoveVec(_, xs) => {
            check_obj_usages(context, xs)?;
            check_gas_by_values(xs)?;
        }
        T::Command__::Publish(_, _, _) => (),
        T::Command__::Upgrade(_, _, _, x, _) => {
            check_obj_usage(context, x)?;
            check_gas_by_value(x)?;
        }
    }
    Ok(())
}

// Checks for valid by-mut-ref and by-value usage of input objects
fn check_obj_usages<'a>(
    context: &mut Context<'_>,
    arguments: &[T::Argument<'_>],
) -> Result<(), ExecutionError<'a>> {
    for arg in arguments {
        check_obj_usage(context, arg)?;
    }
    Ok(())
}

fn check_obj_usage<'a>(
    context: &mut Context<'_>,
    arg: &T::Argument<'_>,
) -> Result<(), ExecutionError<'a>> {
    match &arg.value.0 {
        T::Argument__::Borrow(true, l) => check_obj_by_mut_ref(context, arg.idx, l),
        T::Argument__::Use(T::Usage::Move(l)) => check_by_value(context, arg.idx, l),
        // We do not care about
        // - immutable object borrowing
        // - copying/read ref (since you cannot copy objects)
        // - freeze (since an input object cannot be a reference without a borrow)
        T::Argument__::Borrow(false, _)
        | T::Argument__::Use(T::Usage::Copy { .. })
        | T::Argument__::Read(_)
        | T::Argument__::Freeze(_) => Ok(()),
    }
}

// Checks for valid by-mut-ref usage of input objects
fn check_obj_by_mut_ref<'a>(
    context: &mut Context<'_>,
    arg_idx: u16,
    location: &T::Location,
) -> Result<(), ExecutionError<'a>> {
    match location {
        T::Location::WithdrawalInput(_)
        | T::Location::PureInput(_)
        | T::Location::ReceivingInput(_)
        | T::Location::TxContext
        | T::Location::GasCoin
        | T::Location::Result(_, _) => Ok(()),
        T::Location::ObjectInput(idx) => {
            if !context.objects.safe_get(*idx as usize)?.allow_by_mut_ref {
                Err(command_argument_error(
                    CommandArgumentError::InvalidObjectByMutRef,
                    arg_idx as usize,
                ))
            } else {
                Ok(())
            }
        }
    }
}

// Checks for valid by-value usage of input objects
fn check_by_value<'a>(
    context: &mut Context<'_>,
    arg_idx: u16,
    location: &T::Location,
) -> Result<(), ExecutionError<'a>> {
    match location {
        T::Location::GasCoin
        | T::Location::Result(_, _)
        | T::Location::TxContext
        | T::Location::WithdrawalInput(_)
        | T::Location::PureInput(_)
        | T::Location::ReceivingInput(_) => Ok(()),
        T::Location::ObjectInput(idx) => {
            if !context.objects.safe_get(*idx as usize)?.allow_by_value {
                Err(command_argument_error(
                    CommandArgumentError::InvalidObjectByValue,
                    arg_idx as usize,
                ))
            } else {
                Ok(())
            }
        }
    }
}

// Checks for no by value usage of gas
fn check_gas_by_values<'a>(arguments: &[T::Argument<'_>]) -> Result<(), ExecutionError<'a>> {
    for arg in arguments {
        check_gas_by_value(arg)?;
    }
    Ok(())
}

fn check_gas_by_value<'a>(arg: &T::Argument<'_>) -> Result<(), ExecutionError<'a>> {
    match &arg.value.0 {
        T::Argument__::Use(T::Usage::Move(l)) => check_gas_by_value_loc(arg.idx, l),
        // We do not care about the read/freeze case since they cannot move an object input
        T::Argument__::Borrow(_, _)
        | T::Argument__::Use(T::Usage::Copy { .. })
        | T::Argument__::Read(_)
        | T::Argument__::Freeze(_) => Ok(()),
    }
}

fn check_gas_by_value_loc<'a>(idx: u16, location: &T::Location) -> Result<(), ExecutionError<'a>> {
    match location {
        T::Location::GasCoin => Err(command_argument_error(
            CommandArgumentError::InvalidGasCoinUsage,
            idx as usize,
        )),
        T::Location::TxContext
        | T::Location::ObjectInput(_)
        | T::Location::WithdrawalInput(_)
        | T::Location::PureInput(_)
        | T::Location::ReceivingInput(_)
        | T::Location::Result(_, _) => Ok(()),
    }
}

pub fn is_coin_send_funds(function: &T::LoadedFunction<'_>) -> bool {
    function.original_mid.address() == &SUI_FRAMEWORK_ADDRESS
        && function.original_mid.name() == COIN_MODULE_NAME.as_str()
        && function.name == SEND_FUNDS_FUNC_NAME.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_mode::Normal;
    use crate::static_programmable_transactions::loading::ast::{Datatype, ModuleId, Vector};
    use move_binary_format::file_format::AbilitySet;
    use move_core_types::{account_address::AccountAddress, identifier::IdentStr};

    fn datatype<'a>(
        bump: &'a Bump,
        (address, module, name): (&AccountAddress, &'static IdentStr, &'static IdentStr),
        type_arguments: &[Type<'a>],
    ) -> Type<'a> {
        Type::Datatype(containers::leak(
            bump,
            Datatype {
                abilities: AbilitySet::EMPTY,
                module: ModuleId {
                    address: *address,
                    name: module.as_str(),
                },
                name: name.as_str(),
                type_arguments: containers::alloc_slice_copy(bump, type_arguments),
            },
        ))
    }

    fn check<'a>(bump: &'a Bump, ty: Type<'a>, bytes: &[u8]) -> Option<CommandArgumentError> {
        match check_pure_bytes::<Normal>(bump, 3, bytes, &ty) {
            Ok(()) => None,
            Err(e) => match *e.kind() {
                ExecutionErrorKind::CommandArgumentError { arg_idx: 3, kind } => Some(kind),
                other => panic!("unexpected error kind {other:?}"),
            },
        }
    }

    #[test]
    fn pure_bytes_are_validated_against_the_layout() {
        let bump = Bump::default();
        let bytes = Type::Vector(containers::leak(
            &bump,
            Vector {
                abilities: AbilitySet::EMPTY,
                element_type: Type::U8,
            },
        ));
        assert_eq!(check(&bump, bytes, &[2, 7, 8]), None);
        assert_eq!(
            check(&bump, bytes, &[2, 7]),
            Some(CommandArgumentError::InvalidBCSBytes)
        );
        assert_eq!(
            check(&bump, Type::U64, &[0; 9]),
            Some(CommandArgumentError::InvalidBCSBytes)
        );

        let ascii = datatype(&bump, RESOLVED_ASCII_STR, &[]);
        let option_ascii = datatype(&bump, RESOLVED_STD_OPTION, &[ascii]);
        assert_eq!(check(&bump, option_ascii, &[0]), None);
        assert_eq!(check(&bump, option_ascii, &[1, 2, b'h', b'i']), None);
        assert_eq!(
            check(&bump, option_ascii, &[1, 2, b'h', 0xc3]),
            Some(CommandArgumentError::InvalidBCSBytes)
        );
        let utf8 = datatype(&bump, RESOLVED_UTF8_STR, &[]);
        assert_eq!(
            check(&bump, utf8, &[2, 0xc3, 0x28]),
            Some(CommandArgumentError::InvalidBCSBytes)
        );
        assert_eq!(
            check(&bump, Type::Signer, &[]),
            Some(CommandArgumentError::InvalidUsageOfPureArg)
        );
    }
}
