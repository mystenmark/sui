// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    static_programmable_transactions::{
        env, linkage::resolved_linkage::ExecutableLinkage, typing::ast as T,
    },
};
use exec_types::error::ExecutionError;
pub mod defining_ids_in_types;
pub mod linkage_consistency;
pub mod memory_safety;
pub mod type_check;

pub fn transaction<'a, Mode: ExecutionMode>(
    env: &env::Env<'a, '_, '_, '_, '_, '_, Mode>,
    tt: &T::Transaction<'a>,
    unified_linkage: Option<&ExecutableLinkage<'a>>,
) -> Result<(), ExecutionError<'a>> {
    defining_ids_in_types::verify(env, tt)?;
    type_check::verify::<Mode>(env, tt)?;
    memory_safety::verify(env, tt)?;
    linkage_consistency::verify(env, tt, unified_linkage)?;
    // Add in other invariants checks here as needed/desired.
    Ok(())
}
