// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    error::ExecutionError,
    execution_mode::ExecutionMode,
    gas_charger::GasPayment,
    static_programmable_transactions::{
        env::Env,
        linkage,
        loading::ast::{self as L, PackagePayload},
        metering::{self, translation_meter::TranslationMeter},
    },
};
use containers::Vec;
use exec_types::base::object_ref;
use exec_types::tx_context::TxContext;
use exec_types::type_tags::{to_move_struct_tag_of, to_move_type_tag};
use exec_types::{assert_invariant, invariant_violation, make_invariant_violation};
use messages::base::SuiAddress;
use messages::object::{Owner, Party};
use messages::transaction::{
    self as P, CallArg, FundsWithdrawalArg, ObjectArg, SharedObjectMutability,
};
use move_core_types::{account_address::AccountAddress, u256::U256};
use sui_types::object::ObjectPermissions;

pub fn transaction<'a, Mode: ExecutionMode>(
    meter: &mut TranslationMeter<'_, '_, 'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    tx_context: &TxContext,
    // which inputs are withdrawals that need to be converted to coins, must
    // be the same length as the inputs
    withdrawal_compatibility_inputs: Option<&[bool]>,
    gas_payment: Option<GasPayment>,
    pt: P::ProgrammableTransaction<'a>,
) -> Result<L::Transaction<'a>, ExecutionError<'a>> {
    if env.protocol_config.validate_ptb_argument_indices()
        && let Err(err) = validate_argument_indices(&pt)
    {
        invariant_violation!(
            "PTB argument indices are checked at signing -- this should be impossible: {err}"
        );
    }
    metering::pre_translation::meter(meter, &pt)?;
    let P::ProgrammableTransaction { inputs, commands } = pt;
    // withdrawal_compatibility_inputs specified ==> the protocol config flag is set
    assert_invariant!(
        withdrawal_compatibility_inputs.is_none()
            || env
                .protocol_config
                .convert_withdrawal_compatibility_ptb_arguments(),
        "if withdrawal compatibility must be specified, then the flag is set in the protocol config"
    );
    assert_invariant!(
        withdrawal_compatibility_inputs.is_none_or(|w| inputs.len() == w.len()),
        "withdrawal compatibility inputs must be the same length as the inputs"
    );
    let mut loaded_inputs = Vec::with_capacity_in(inputs.len(), env.bump);
    for (idx, arg) in inputs.iter().enumerate() {
        let is_withdrawal_compatibility_input =
            withdrawal_compatibility_inputs.is_some_and(|w| w.get(idx).copied().unwrap_or(false));
        loaded_inputs.push(input(
            env,
            tx_context,
            is_withdrawal_compatibility_input,
            *arg,
        )?);
    }
    let original_command_len = commands.len();
    let mut loaded_commands = Vec::with_capacity_in(commands.len(), env.bump);
    for (idx, cmd) in commands.iter().enumerate() {
        loaded_commands.push(command(env, cmd).map_err(|e| e.with_command_index(idx))?);
    }
    let loaded_tx = L::Transaction {
        gas_payment,
        inputs: loaded_inputs,
        original_command_len,
        commands: loaded_commands,
        // Set by `linkage::refine_linkage` below if unified linkage is enabled.
        unified_linkage: None,
    };
    metering::loading::meter(meter, &loaded_tx)?;
    linkage::refine_linkage(
        loaded_tx,
        env.linkage_analysis,
        env.linkable_store,
        env.protocol_config,
    )
}

/// `ProgrammableTransaction::validate_argument_indices`: `Input` must name an input and
/// `Result`/`NestedResult` an earlier command.
fn validate_argument_indices(pt: &P::ProgrammableTransaction<'_>) -> Result<(), String> {
    for (command_idx, command) in pt.commands.iter().enumerate() {
        for (argument_idx, argument) in command_arguments(command).enumerate() {
            let index = match argument {
                P::Argument::Input(index) if index as usize >= pt.inputs.len() => index,
                P::Argument::Result(index) | P::Argument::NestedResult(index, _)
                    if index as usize >= command_idx =>
                {
                    index
                }
                P::Argument::GasCoin
                | P::Argument::Input(_)
                | P::Argument::Result(_)
                | P::Argument::NestedResult(_, _) => continue,
            };
            return Err(format!(
                "Invalid argument index {index} in command {command_idx}, argument {argument_idx}"
            ));
        }
    }
    Ok(())
}

/// `Command::arguments`.
fn command_arguments<'b>(command: &P::Command<'b>) -> impl Iterator<Item = P::Argument> + 'b {
    let (first, rest): (Option<P::Argument>, &'b [P::Argument]) = match *command {
        P::Command::MoveCall(ref call) => (None, call.arguments),
        P::Command::TransferObjects(objects, address) => (Some(address), objects),
        P::Command::SplitCoins(coin, amounts) => (Some(coin), amounts),
        P::Command::MergeCoins(target, coins) => (Some(target), coins),
        P::Command::MakeMoveVec(_, elements) => (None, elements),
        P::Command::Publish(_, _) => (None, &[]),
        P::Command::Upgrade(_, _, _, ticket) => (Some(ticket), &[]),
    };
    // The reference lists `TransferObjects`' objects before its address, and the other
    // commands' single argument first.
    let (before, after) = match command {
        P::Command::TransferObjects(_, _) => (None, first),
        _ => (first, None),
    };
    before.into_iter().chain(rest.iter().copied()).chain(after)
}

fn input<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    tx_context: &TxContext,
    // True iff this is a withdrawal that needs to be converted to a coin
    is_withdrawal_compatibility_input: bool,
    arg: CallArg<'a>,
) -> Result<(L::InputArg<'a>, L::InputType<'a>), ExecutionError<'a>> {
    // is_withdrawal_compatibility_input ==> FundsWithdrawal
    assert_invariant!(
        !is_withdrawal_compatibility_input || matches!(arg, CallArg::FundsWithdrawal(_)),
        "withdrawal compatibility inputs must be FundsWithdrawal"
    );
    Ok(match arg {
        CallArg::Pure(bytes) => (L::InputArg::Pure(bytes), L::InputType::Bytes),
        CallArg::Object(ObjectArg::Receiving(oref)) => (
            L::InputArg::Receiving(object_ref(oref)),
            L::InputType::Bytes,
        ),
        CallArg::Object(ObjectArg::ImmOrOwnedObject(oref)) => {
            let oref = object_ref(oref);
            let id = &oref.0;
            let obj = env.read_object(id)?;
            let Some(ty) = obj.type_() else {
                invariant_violation!("Object {:?} has does not have a Move type", id);
            };
            let tag = to_move_struct_tag_of(ty);
            let ty = env.load_type_from_struct(&tag)?;
            let arg = match obj.owner() {
                Owner::AddressOwner(_) => L::ObjectArg {
                    kind: L::ObjectArgKind::OwnedObject(oref),
                    refined_permissions: ObjectPermissions::ALL,
                },
                Owner::Immutable => L::ObjectArg {
                    kind: L::ObjectArgKind::ImmObject(oref),
                    refined_permissions: ObjectPermissions::IMMUTABLE_USAGE,
                },
                Owner::ObjectOwner(_)
                | Owner::Shared { .. }
                | Owner::ConsensusAddressOwner { .. } => {
                    assert_invariant!(
                        Mode::allow_arbitrary_values(),
                        "Unexpected owner for ImmOrOwnedObject: {:?}",
                        obj.owner(),
                    );
                    let kind = L::ObjectArgKind::OwnedObject(oref);
                    L::ObjectArg {
                        kind,
                        refined_permissions: ObjectPermissions::ALL,
                    }
                }
                Owner::Party(party) => {
                    assert_invariant!(
                        Mode::allow_arbitrary_values(),
                        "Unexpected owner for ImmOrOwnedObject: {:?}",
                        obj.owner(),
                    );
                    let refined_permissions = permissions_for(party, &tx_context.sender())?;
                    L::ObjectArg {
                        kind: L::ObjectArgKind::OwnedObject(oref),
                        refined_permissions,
                    }
                }
            };
            (L::InputArg::Object(arg), L::InputType::Fixed(ty))
        }
        CallArg::Object(ObjectArg::SharedObject(shared)) => {
            let id = shared.id;
            let initial_shared_version = shared.initial_shared_version.get();
            let mutability = shared.mutability();
            let obj = env.read_object(&id)?;
            let Some(ty) = obj.type_() else {
                invariant_violation!("Object {:?} does not have a Move type", id);
            };
            let tag = to_move_struct_tag_of(ty);
            let ty = env.load_type_from_struct(&tag)?;
            let owner_permissions = match obj.owner() {
                Owner::AddressOwner(_) | Owner::ObjectOwner(_) | Owner::Immutable => {
                    assert_invariant!(
                        Mode::allow_arbitrary_values(),
                        "Unexpected owner for SharedObject: {:?}",
                        obj.owner()
                    );
                    ObjectPermissions::ALL
                }
                Owner::Shared { .. } => ObjectPermissions::LEGACY_SHARED_OBJECT,
                Owner::ConsensusAddressOwner { .. } => ObjectPermissions::ALL,
                Owner::Party(party) => permissions_for(party, &tx_context.sender())?,
            };
            let refined_permissions = refine_permissions(mutability, owner_permissions)?;
            let kind = L::ObjectArgKind::ConsensusObject {
                id,
                initial_shared_version,
            };
            (
                L::InputArg::Object(L::ObjectArg {
                    kind,
                    refined_permissions,
                }),
                L::InputType::Fixed(ty),
            )
        }
        CallArg::FundsWithdrawal(f) => {
            assert_invariant!(
                env.protocol_config.enable_accumulators(),
                "Withdrawals should be rejected at signing if accumulators are not enabled"
            );
            let FundsWithdrawalArg {
                reservation,
                type_arg,
                withdraw_from,
            } = *f;
            let amount = match reservation {
                P::Reservation::MaxAmountU64(u) => U256::from(u),
                // TODO when types other than u64 are supported, we must check that this is a
                // valid amount for the type
            };
            let funds_ty = match type_arg {
                P::WithdrawalTypeArg::Balance(inner) => {
                    let inner = env.load_type_tag(0, &to_move_type_tag(&inner))?;
                    env.balance_type(inner)?
                }
            };
            let source = match withdraw_from {
                P::WithdrawFrom::Sender => L::WithdrawalSource::Direct {
                    owner: account_address(&tx_context.sender()),
                },
                P::WithdrawFrom::Sponsor => L::WithdrawalSource::Direct {
                    owner: account_address(&tx_context.sponsor().ok_or_else(|| {
                        make_invariant_violation!(
                            "A sponsor withdrawal requires a sponsor and should have been \
                                checked at signing"
                        )
                    })?),
                },
                P::WithdrawFrom::SenderAllowance { funder, allowance } => {
                    L::WithdrawalSource::Allowance {
                        funder: account_address(funder),
                        id: *allowance,
                    }
                }
            };
            let ty = env.withdrawal_type_for_source(&source, funds_ty)?;
            (
                L::InputArg::FundsWithdrawal(L::FundsWithdrawalArg {
                    from_compatibility_object: is_withdrawal_compatibility_input,
                    amount,
                    ty,
                    source,
                }),
                L::InputType::Fixed(ty),
            )
        }
    })
}

fn account_address(address: &SuiAddress) -> AccountAddress {
    AccountAddress::new(address.0)
}

/// `Party::permissions_for`. A stored party's members are sorted and unique, and its
/// permissions valid, as its deserialization in the reference requires.
fn permissions_for(
    party: &Party<'_>,
    address: &SuiAddress,
) -> Result<ObjectPermissions, ExecutionError<'static>> {
    let member = party
        .members
        .binary_search_by(|member| member.address.cmp(address))
        .ok()
        .and_then(|i| party.members.get(i));
    let bits = member.map_or(party.default_permissions, |m| m.permissions.get());
    let Some(permissions) = ObjectPermissions::new(bits) else {
        invariant_violation!("Party permissions {bits:#x} are not valid");
    };
    Ok(permissions)
}

fn refine_permissions(
    mutability: SharedObjectMutability,
    permissions: ObjectPermissions,
) -> Result<ObjectPermissions, ExecutionError<'static>> {
    Ok(match mutability {
        SharedObjectMutability::Mutable | SharedObjectMutability::NonExclusiveWrite => {
            assert_invariant!(
                permissions.can_use_mutably(),
                "Mutable shared object usage requires mutable usage permission"
            );
            permissions
        }
        SharedObjectMutability::Immutable => {
            assert_invariant!(
                permissions.can_use_immutably(),
                "Immutable shared object usage requires immutable usage permission"
            );
            ObjectPermissions::IMMUTABLE_USAGE
        }
    })
}

fn command<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    command: &P::Command<'a>,
) -> Result<L::Command<'a>, ExecutionError<'a>> {
    let bump = env.bump;
    let args = |args: &[P::Argument]| containers::vec_from_slice(bump, args);
    Ok(match *command {
        P::Command::MoveCall(ref pmc) => {
            let P::ProgrammableMoveCall {
                package,
                module,
                function: name,
                type_arguments: ptype_arguments,
                arguments,
            } = *pmc;
            let mut type_arguments = Vec::with_capacity_in(ptype_arguments.len(), bump);
            for (idx, ty) in ptype_arguments.iter().enumerate() {
                type_arguments.push(env.load_type_input(idx, *ty)?);
            }
            let function = env.load_function(*package, module, name, type_arguments)?;
            L::Command::MoveCall(containers::Box::new_in(
                L::MoveCall {
                    function,
                    arguments: args(arguments),
                },
                bump,
            ))
        }
        P::Command::MakeMoveVec(ptype_argument, arguments) => {
            let type_argument = ptype_argument
                .map(|ty| env.load_type_input(0, ty))
                .transpose()?;
            L::Command::MakeMoveVec(type_argument, args(arguments))
        }
        P::Command::TransferObjects(objects, address) => {
            L::Command::TransferObjects(args(objects), address)
        }
        P::Command::SplitCoins(coin, amounts) => L::Command::SplitCoins(coin, args(amounts)),
        P::Command::MergeCoins(target, coins) => L::Command::MergeCoins(target, args(coins)),
        P::Command::Publish(items, dep_ids) => {
            let resolved_linkage = env
                .linkage_analysis
                .compute_publication_linkage(dep_ids, env.linkable_store)?;
            let payload = if env.protocol_config.enable_unified_linkage() {
                let deserialized_pkg = env.deserialize_package(items, dep_ids)?;
                PackagePayload::Deserialized(deserialized_pkg)
            } else {
                PackagePayload::Serialized(items)
            };
            L::Command::Publish(
                payload,
                containers::vec_from_slice(bump, dep_ids),
                resolved_linkage,
            )
        }
        P::Command::Upgrade(items, dep_ids, object_id, argument) => {
            let resolved_linkage = env
                .linkage_analysis
                .compute_publication_linkage(dep_ids, env.linkable_store)?;
            let payload = if env.protocol_config.enable_unified_linkage() {
                let deserialized_pkg = env.deserialize_package(items, dep_ids)?;
                PackagePayload::Deserialized(deserialized_pkg)
            } else {
                PackagePayload::Serialized(items)
            };
            L::Command::Upgrade(
                payload,
                containers::vec_from_slice(bump, dep_ids),
                *object_id,
                argument,
                resolved_linkage,
            )
        }
    })
}
