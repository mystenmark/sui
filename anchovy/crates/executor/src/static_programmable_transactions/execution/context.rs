// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use exec_types::error::{ExecutionError, command_argument_error};
use messages::execution_status::CommandArgumentError;
use move_binary_format::errors::{Location, PartialVMResult, VMResult};
use move_vm_runtime::execution::{Type as VMType, TypeSubst as _, vm::LoadedFunctionInformation};

pub fn subst_signature(
    signature: LoadedFunctionInformation,
    type_arguments: &[VMType],
) -> VMResult<LoadedFunctionInformation> {
    let LoadedFunctionInformation {
        parameters,
        return_,
        is_entry,
        is_native,
        visibility,
        index,
        instruction_count,
    } = signature;
    let parameters = parameters
        .into_iter()
        .map(|ty| ty.subst(type_arguments))
        .collect::<PartialVMResult<Vec<_>>>()
        .map_err(|err| err.finish(Location::Undefined))?;
    let return_ = return_
        .into_iter()
        .map(|ty| ty.subst(type_arguments))
        .collect::<PartialVMResult<Vec<_>>>()
        .map_err(|err| err.finish(Location::Undefined))?;
    Ok(LoadedFunctionInformation {
        parameters,
        return_,
        is_entry,
        is_native,
        visibility,
        index,
        instruction_count,
    })
}

pub enum EitherError<'a> {
    CommandArgument(CommandArgumentError),
    Execution(ExecutionError<'a>),
}

impl<'a> From<ExecutionError<'a>> for EitherError<'a> {
    fn from(e: ExecutionError<'a>) -> Self {
        EitherError::Execution(e)
    }
}

impl From<CommandArgumentError> for EitherError<'_> {
    fn from(e: CommandArgumentError) -> Self {
        EitherError::CommandArgument(e)
    }
}

impl<'a> EitherError<'a> {
    pub fn into_execution_error(self, command_index: usize) -> ExecutionError<'a> {
        match self {
            EitherError::CommandArgument(e) => command_argument_error(e, command_index),
            EitherError::Execution(e) => e,
        }
    }
}
