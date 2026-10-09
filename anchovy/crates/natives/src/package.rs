// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{get_extension, object_runtime::ObjectRuntime};
use exec_types::base::{move_address, object_id};
use exec_types::object::move_package_size;
use messages::object::MovePackage;
use move_binary_format::CompiledModule;
use move_binary_format::{checked_as, safe_assert_eq, safe_unwrap};
use move_core_types::{account_address::AccountAddress, gas_algebra::InternalGas};
use move_vm_runtime::{
    execution::values::Value,
    native_charge_gas_early_exit,
    natives::functions::{NativeContext, NativeResult},
    pop_arg,
};
use smallvec::smallvec;
use std::collections::VecDeque;

const E_INVALID_PACKAGE_VERSION: u64 = 6;

/// `OBJECT_START_VERSION`: the version objects and packages are created at.
const OBJECT_START_VERSION: u64 = 1;

#[derive(Clone)]
pub struct PackageVersioningOriginalPackageIdImplCostParams {
    pub package_original_package_id_impl_cost_base: Option<InternalGas>,
    pub package_original_package_id_impl_cost_per_byte: Option<InternalGas>,
}

pub fn original_package_id_impl(
    context: &mut NativeContext,
    ty_args: Vec<move_vm_runtime::execution::Type>,
    mut args: VecDeque<Value>,
) -> move_binary_format::errors::PartialVMResult<NativeResult> {
    safe_assert_eq!(ty_args.len(), 0);
    safe_assert_eq!(args.len(), 2);

    let PackageVersioningOriginalPackageIdImplCostParams {
        package_original_package_id_impl_cost_base,
        package_original_package_id_impl_cost_per_byte,
    } = get_extension!(context, crate::NativesCostTable)?
        .package_original_package_id_impl_cost_params
        .clone();

    let package_original_package_id_impl_cost_base =
        safe_unwrap!(package_original_package_id_impl_cost_base);

    let package_original_package_id_impl_cost_per_byte =
        safe_unwrap!(package_original_package_id_impl_cost_per_byte);

    native_charge_gas_early_exit!(context, package_original_package_id_impl_cost_base);

    let version = pop_arg!(args, u64);
    let package_id = object_id(&pop_arg!(args, AccountAddress));
    let Some(package) =
        get_extension!(context, ObjectRuntime)?.get_package_at_version(package_id, version)
    else {
        return Ok(NativeResult::err(
            context.gas_used(),
            E_INVALID_PACKAGE_VERSION,
        ));
    };
    native_charge_gas_early_exit!(
        context,
        package_original_package_id_impl_cost_per_byte
            * checked_as!(move_package_size(package.try_as_package().unwrap()), u64)?.into()
    );
    let original_id = original_package_id(package.try_as_package().unwrap());

    Ok(NativeResult::ok(
        context.gas_used(),
        smallvec![Value::address(original_id)],
    ))
}

/// `MovePackage::original_package_id`: the id a package was first published at, which every
/// version's modules carry as their address.
fn original_package_id(package: &MovePackage<'_>) -> AccountAddress {
    if package.version == OBJECT_START_VERSION {
        // for a non-upgraded package, original ID is just the package ID
        return move_address(package.id);
    }
    // The reference takes the first module in name order and this the first in encoded order,
    // but every module of a package carries the same address.
    let (_, bytes) = package.module_map.first().expect("Empty module map");
    let module = CompiledModule::deserialize_with_defaults(bytes)
        .expect("A Move package contains a module that cannot be deserialized");
    *module.address()
}
