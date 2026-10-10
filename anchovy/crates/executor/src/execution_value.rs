// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use exec_types::storage::{ObjectFundsResolver, RuntimeObjectResolver};

use crate::storage::Storage;

/// Interface with the store necessary to execute a programmable transaction
pub trait ExecutionState<'a>:
    Storage<'a> + RuntimeObjectResolver<'a> + ObjectFundsResolver
{
}

impl<'a, T> ExecutionState<'a> for T where
    T: Storage<'a> + RuntimeObjectResolver<'a> + ObjectFundsResolver
{
}
