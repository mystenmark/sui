// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::accumulator_event`.

use messages::base::ObjectId;
use messages::effects::{AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1};
use messages::type_tag::TypeTag;

use exec_types::base::SUI_FRAMEWORK_ADDRESS;

#[derive(Debug, Clone, Copy)]
pub struct AccumulatorEvent<'a> {
    /// The reference's `AccumulatorObjId`: the id of the accumulator's field object.
    pub accumulator_obj: ObjectId,
    pub write: AccumulatorWriteV1<'a>,
}

impl<'a> AccumulatorEvent<'a> {
    pub fn new(accumulator_obj: ObjectId, write: AccumulatorWriteV1<'a>) -> Self {
        Self {
            accumulator_obj,
            write,
        }
    }

    pub fn total_sui_in_event(&self) -> (u64 /* input */, u64 /* output */) {
        let Self {
            write:
                AccumulatorWriteV1 {
                    ty,
                    operation,
                    value,
                    ..
                },
            ..
        } = self;

        let sui = match ty {
            TypeTag::Struct(struct_tag) => {
                if !is_gas_balance(struct_tag) {
                    0
                } else {
                    match value {
                        AccumulatorValue::Integer(v) => *v,
                        AccumulatorValue::IntegerTuple(_, _) => {
                            panic!("invalid accumulator value")
                        }
                        AccumulatorValue::EventDigest(_) => panic!("invalid accumulator value"),
                    }
                }
            }
            _ => 0,
        };

        match operation {
            AccumulatorOperation::Merge => (0, sui),
            AccumulatorOperation::Split => (sui, 0),
        }
    }
}

/// `GasCoin::is_gas_balance`: `0x2::balance::Balance<0x2::sui::SUI>`.
pub fn is_gas_balance(s: &messages::type_tag::StructTag<'_>) -> bool {
    *s.address == SUI_FRAMEWORK_ADDRESS
        && s.module == "balance"
        && s.name == "Balance"
        && s.type_params.len() == 1
        && is_gas_type(&s.type_params[0])
}

/// `GAS::is_gas_type`: `0x2::sui::SUI`.
pub fn is_gas_type(t: &TypeTag<'_>) -> bool {
    matches!(t, TypeTag::Struct(s)
        if *s.address == SUI_FRAMEWORK_ADDRESS
            && s.module == "sui"
            && s.name == "SUI"
            && s.type_params.is_empty())
}

/// `sui_types::balance_change::signed_balance_changes_from_events`: each `Balance<T>` integer
/// event's address, `T` and signed amount.
pub fn signed_balance_changes_from_events<'b, 'a>(
    events: &'b [AccumulatorEvent<'a>],
) -> impl Iterator<Item = (messages::base::SuiAddress, TypeTag<'a>, i128)> + 'b {
    events.iter().filter_map(signed_balance_change_from_event)
}

/// Extract the signed balance change from a single accumulator event, if it
/// has a `Balance<T>` type and an integer value.
fn signed_balance_change_from_event<'a>(
    event: &AccumulatorEvent<'a>,
) -> Option<(messages::base::SuiAddress, TypeTag<'a>, i128)> {
    let ty = &event.write.ty;
    // Only process events with Balance<T> types
    let coin_type = crate::accumulator_root::maybe_get_balance_type_param(ty)?;

    let amount = match &event.write.value {
        AccumulatorValue::Integer(v) => *v as i128,
        // IntegerTuple and EventDigest are not balance-related
        AccumulatorValue::IntegerTuple(_, _) | AccumulatorValue::EventDigest(_) => {
            return None;
        }
    };

    // Convert operation to signed amount: Split means balance decreased, Merge means increased
    let signed_amount = match event.write.operation {
        AccumulatorOperation::Split => -amount,
        AccumulatorOperation::Merge => amount,
    };

    Some((*event.write.address, coin_type, signed_amount))
}
