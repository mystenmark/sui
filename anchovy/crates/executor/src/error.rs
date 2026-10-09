// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The adapter's `error`, with `sui_types::error::ExecutionError` (from
//! `exec-types`) alongside.

use containers::{Bump, alloc, alloc_str};
use move_binary_format::{
    errors::{Location, VMError},
    file_format::FunctionDefinitionIndex,
};
use move_core_types::{
    language_storage::ModuleId,
    vm_status::{StatusCode, StatusType},
};

pub use exec_types::error::*;
use messages::base::AccountAddress;
use messages::execution_status::{self, MoveLocation};

/// A VM module id as the view an execution error holds, in `bump`.
fn module_id_in<'a>(bump: &'a Bump, id: &ModuleId) -> execution_status::ModuleId<'a> {
    execution_status::ModuleId {
        address: alloc(bump, AccountAddress(id.address().into_bytes())),
        name: alloc_str(bump, id.name().as_str()),
    }
}

pub(crate) fn convert_vm_error_impl<'a>(
    bump: &'a Bump,
    error: VMError,
    abort_module_id_relocation_fn: &impl Fn(&ModuleId) -> ModuleId,
    function_name_resolution_fn: &impl Fn(&ModuleId, FunctionDefinitionIndex) -> Option<String>,
) -> ExecutionError<'a> {
    let kind = match (error.major_status(), error.sub_status(), error.location()) {
        (StatusCode::EXECUTED, _, _) => {
            // If we have an error the status probably shouldn't ever be Executed
            debug_assert!(false, "VmError shouldn't ever report successful execution");
            ExecutionErrorKind::VMInvariantViolation
        }
        (StatusCode::ABORTED, None, _) => {
            debug_assert!(false, "No abort code");
            // this is a Move VM invariant violation, the code should always be there
            ExecutionErrorKind::VMInvariantViolation
        }
        (StatusCode::ABORTED, Some(code), Location::Module(id)) => {
            let abort_location_id = abort_module_id_relocation_fn(id);
            let offset = error.offsets().first().copied().map(|(f, i)| (f.0, i));
            debug_assert!(offset.is_some(), "Move should set the location on aborts");
            let (function, instruction) = offset.unwrap_or((0, 0));
            let function_name = function_name_resolution_fn(id, FunctionDefinitionIndex(function));
            ExecutionErrorKind::MoveAbort(
                MoveLocation {
                    module: module_id_in(bump, &abort_location_id),
                    function,
                    instruction,
                    function_name: function_name.map(|name| alloc_str(bump, &name)),
                },
                code,
            )
        }
        (StatusCode::OUT_OF_GAS, _, _) => ExecutionErrorKind::InsufficientGas,
        (_, _, location) => match error.major_status().status_type() {
            StatusType::Execution => {
                debug_assert!(error.major_status() != StatusCode::ABORTED);
                let location = match location {
                    Location::Module(id) => {
                        let offset = error.offsets().first().copied().map(|(f, i)| (f.0, i));
                        debug_assert!(
                            offset.is_some(),
                            "Move should set the location on all execution errors. Error {error}"
                        );
                        let (function, instruction) = offset.unwrap_or((0, 0));
                        let function_name =
                            function_name_resolution_fn(id, FunctionDefinitionIndex(function));
                        Some(MoveLocation {
                            module: module_id_in(bump, id),
                            function,
                            instruction,
                            function_name: function_name.map(|name| alloc_str(bump, &name)),
                        })
                    }
                    _ => None,
                };
                ExecutionErrorKind::MovePrimitiveRuntimeError(location)
            }
            StatusType::Validation
            | StatusType::Verification
            | StatusType::Deserialization
            | StatusType::Unknown => ExecutionErrorKind::VMVerificationOrDeserializationError,
            StatusType::InvariantViolation => ExecutionErrorKind::VMInvariantViolation,
        },
    };
    ExecutionError::new_with_source(kind, error)
}
