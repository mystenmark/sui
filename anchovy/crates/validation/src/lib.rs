// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Static transaction validation: the checks that depend only on a
//! transaction's bytes and on epoch-level inputs. Each function mirrors one
//! reference function and applies its checks in the same order, so that the
//! first failure, and so the error, is the same.

pub mod accumulator;
pub mod error;
pub mod gasless;
pub mod kind;
pub mod transaction_data;

use messages::base::ChainIdentifier;
use protocol_config::ProtocolConfig;

pub use error::{Error, ErrorKind};

/// The epoch-level inputs, as the reference's `TxValidityCheckContext`.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub config: &'a ProtocolConfig,
    pub epoch: u64,
    pub chain_identifier: ChainIdentifier,
    pub reference_gas_price: u64,
    /// Validators in the epoch's committee, which bounds committee indices.
    pub committee_size: u32,
}
