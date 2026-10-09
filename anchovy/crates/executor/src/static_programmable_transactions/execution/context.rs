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

/// There is validation to do on top of the BCS layout. Currently only needed for
/// strings
// The reference boxes the inner layouts; here they are in the transaction's arena.
#[derive(Debug, Clone, Copy)]
pub enum PrimitiveArgumentLayout<'a> {
    /// An option
    Option(&'a PrimitiveArgumentLayout<'a>),
    /// A vector
    Vector(&'a PrimitiveArgumentLayout<'a>),
    /// An ASCII encoded string
    Ascii,
    /// A UTF8 encoded string
    UTF8,
    // needed for Option validation
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
}
