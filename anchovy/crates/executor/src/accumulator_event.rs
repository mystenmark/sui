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
