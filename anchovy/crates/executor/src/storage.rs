// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::storage` only the executor uses.

use containers::{BTreeMap, BTreeSet};
use exec_types::error::ExecutionError;
use exec_types::execution::DynamicallyLoadedObjectMetadata;
use exec_types::object::Object;
use messages::base::{ObjectId, SuiAddress};
use messages::type_tag::TypeTag;

use crate::execution::ExecutionResultsV2;

pub struct DenyListResult<'a> {
    /// Ok if all regulated coin owners are allowed.
    /// Err if any regulated coin owner is denied (returning the error for first one denied).
    pub result: Result<(), ExecutionError<'a>>,
    /// The number of non-gas-coin owners in the transaction results
    pub num_non_gas_coin_owners: u64,
}

/// An abstraction of the (possibly distributed) store for objects, and (soon) events and transactions
pub trait Storage<'a> {
    fn reset(&mut self);

    fn read_object(&self, id: &ObjectId) -> Option<&Object<'a>>;

    fn record_execution_results(
        &mut self,
        results: ExecutionResultsV2<'a>,
    ) -> Result<(), ExecutionError<'a>>;

    fn save_loaded_runtime_objects(
        &mut self,
        loaded_runtime_objects: BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    );

    fn save_wrapped_object_containers(
        &mut self,
        wrapped_object_containers: BTreeMap<'a, ObjectId, ObjectId>,
    );

    /// Given the set of all coin types and owners that are receiving the coins during execution,
    /// Check coin denylist v2, and return the number of non-gas-coin owners.
    fn check_coin_deny_list(
        &self,
        receiving_funds_type_and_owners: BTreeMap<'a, TypeTag<'a>, BTreeSet<'a, SuiAddress>>,
    ) -> DenyListResult<'a>;

    fn record_generated_object_ids(&mut self, generated_ids: BTreeSet<'a, ObjectId>);
}
