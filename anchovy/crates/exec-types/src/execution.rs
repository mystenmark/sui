// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::execution` the natives and the executor share.

use messages::base::{ObjectDigest, SequenceNumber, TransactionDigest};
use messages::object::Owner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicallyLoadedObjectMetadata<'a> {
    pub version: SequenceNumber,
    pub digest: ObjectDigest,
    pub owner: Owner<'a>,
    pub storage_rebate: u64,
    pub previous_transaction: TransactionDigest,
}
