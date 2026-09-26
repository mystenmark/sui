// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What the validator knows about the current epoch. Fixed at startup for
//! now; replaced whole at reconfiguration later, so readers hold an `Arc`.

use messages::base::Digest;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};

pub struct EpochState {
    pub config: ProtocolConfig,
    pub chain: Chain,
    pub epoch: u64,
    /// The chain's genesis checkpoint digest.
    pub chain_identifier: Digest,
    pub reference_gas_price: u64,
    pub committee_size: u32,
}

impl EpochState {
    pub fn new(
        chain: Chain,
        protocol_version: u64,
        epoch: u64,
        chain_identifier: Digest,
        reference_gas_price: u64,
        committee_size: u32,
    ) -> EpochState {
        EpochState {
            config: ProtocolConfig::get_for_version(ProtocolVersion::new(protocol_version), chain),
            chain,
            epoch,
            chain_identifier,
            reference_gas_price,
            committee_size,
        }
    }

    pub fn context(&self) -> validation::Context<'_> {
        validation::Context {
            config: &self.config,
            epoch: self.epoch,
            chain_identifier: self.chain_identifier,
            reference_gas_price: self.reference_gas_price,
            committee_size: self.committee_size,
        }
    }
}
