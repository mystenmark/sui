// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Only the helpers the invariant checks use; the rest of the reference's `input_arguments` is
//! not ported yet.

use crate::static_programmable_transactions::{
    execution::context::PrimitiveArgumentLayout, loading::ast::Type,
};
use containers::Bump;
use exec_types::{error::ExecutionError, invariant_violation};
use sui_types::{
    base_types::{RESOLVED_ASCII_STR, RESOLVED_STD_OPTION, RESOLVED_UTF8_STR},
    id::RESOLVED_SUI_ID,
    transfer::RESOLVED_RECEIVING_STRUCT,
};

fn primitive_serialization_layout<'a>(
    bump: &'a Bump,
    param_ty: &Type,
) -> Result<Option<PrimitiveArgumentLayout<'a>>, ExecutionError<'static>> {
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

pub fn is_valid_pure_type(bump: &Bump, constraint: &Type) -> Result<bool, ExecutionError<'static>> {
    Ok(primitive_serialization_layout(bump, constraint)?.is_some())
}

/// Returns true if a type is a `Receiving<t>` where `t` has `key`
pub fn is_valid_receiving(constraint: &Type) -> bool {
    let Type::Datatype(dt) = constraint else {
        return false;
    };
    dt.is_resolved(RESOLVED_RECEIVING_STRUCT)
        && dt.type_arguments.len() == 1
        && dt.type_arguments.first().unwrap().abilities().has_key()
}
