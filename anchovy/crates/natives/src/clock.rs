// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{NativesCostTable, get_extension, get_extension_mut, object_runtime::ObjectRuntime};
use move_binary_format::errors::{PartialVMError, PartialVMResult};
use move_binary_format::{safe_assert, safe_unwrap};
use move_core_types::{gas_algebra::InternalGas, language_storage::TypeTag, vm_status::StatusCode};
use move_vm_runtime::{
    execution::{Type, values::Value},
    native_charge_gas_early_exit,
    natives::functions::{NativeContext, NativeResult},
};
use smallvec::smallvec;
use std::collections::VecDeque;
use sui_types::clock::Clock;

#[derive(Clone)]
pub struct ClockBorrowCostParams {
    pub clock_borrow_cost_base: Option<InternalGas>,
}
/***************************************************************************************************
 * native fun native_borrow
 * Implementation of the Move native function `fun native_borrow(): &Clock`
 *   gas cost: clock_borrow_cost_base | fixed cost; the Clock is a fixed-size object
 **************************************************************************************************/
pub fn borrow(
    context: &mut NativeContext,
    ty_args: Vec<Type>,
    args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    safe_assert!(ty_args.is_empty());
    safe_assert!(args.is_empty());

    let clock_borrow_cost_base = safe_unwrap!(
        get_extension!(context, NativesCostTable)?
            .clock_borrow_cost_params
            .clock_borrow_cost_base
    );
    native_charge_gas_early_exit!(context, clock_borrow_cost_base);

    let layout = context
        .type_tag_to_type_layout(&TypeTag::Struct(Box::new(Clock::type_())))
        .ok_or_else(|| {
            PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                .with_message("Failed to load the Clock type layout".to_string())
        })?;
    let object_runtime: &mut ObjectRuntime = get_extension_mut!(context)?;
    let clock_ref = object_runtime.borrow_clock(&layout)?;

    Ok(NativeResult::ok(context.gas_used(), smallvec![clock_ref]))
}
