// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::error::ExecutionError;
use crate::static_programmable_transactions::{
    linkage::resolved_linkage::ResolvedLinkage, loading::ast as L,
    metering::translation_meter::TranslationMeter,
};

/// After loading and before type checking we do a pass over the loaded transaction to charge for
/// types that occured in the transaction and were loaded. We simply charge for the number of type
/// nodes that were loaded.
pub fn meter<'a>(
    meter: &mut TranslationMeter<'_, '_, 'a>,
    transaction: &L::Transaction<'_>,
) -> Result<(), ExecutionError<'a>> {
    let inputs = transaction.inputs.iter().filter_map(|i| match &i.1 {
        L::InputType::Bytes => None,
        L::InputType::Fixed(ty) => Some(ty),
    });
    let commands = transaction.commands.iter().flat_map(command_types);
    for ty in inputs.chain(commands) {
        meter.charge_num_type_nodes(ty.node_count())?;
    }

    for linkage in transaction.commands.iter().filter_map(command_linkage) {
        meter.charge_num_linkage_entries(linkage.linkage_resolution.len())?;
    }

    for cmd in &transaction.commands {
        match cmd {
            L::Command::Publish(payload, _, _) | L::Command::Upgrade(payload, _, _, _, _) => {
                meter.charge_package_load(payload)?;
            }
            L::Command::MoveCall(_)
            | L::Command::MakeMoveVec(_, _)
            | L::Command::TransferObjects(_, _)
            | L::Command::SplitCoins(_, _)
            | L::Command::MergeCoins(_, _) => (),
        }
    }

    Ok(())
}

fn command_linkage<'b, 'a>(cmd: &'b L::Command<'a>) -> Option<&'b ResolvedLinkage<'a>> {
    match cmd {
        L::Command::Publish(_, _, linkage) | L::Command::Upgrade(_, _, _, _, linkage) => {
            Some(linkage)
        }
        L::Command::MoveCall(call) => Some(call.function.linkage.0),
        L::Command::MakeMoveVec(_, _)
        | L::Command::TransferObjects(_, _)
        | L::Command::SplitCoins(_, _)
        | L::Command::MergeCoins(_, _) => None,
    }
}

// The reference boxes the iterator; chaining options keeps it off the heap.
fn command_types<'b, 'a>(cmd: &'b L::Command<'a>) -> impl Iterator<Item = &'b L::Type<'a>> {
    let (call, vec_ty) = match cmd {
        L::Command::MoveCall(move_call) => (Some(move_call), None),
        L::Command::MakeMoveVec(Some(ty), _) => (None, Some(ty)),
        L::Command::TransferObjects(_, _)
        | L::Command::SplitCoins(_, _)
        | L::Command::MergeCoins(_, _)
        | L::Command::MakeMoveVec(None, _)
        | L::Command::Publish(_, _, _)
        | L::Command::Upgrade(_, _, _, _, _) => (None, None),
    };
    call.into_iter()
        .flat_map(|move_call| {
            move_call
                .function
                .type_arguments
                .iter()
                .chain(move_call.function.signature.parameters.iter())
                .chain(move_call.function.signature.return_.iter())
        })
        .chain(vec_ty)
}
