// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What the validator knows about the current epoch. Fixed at startup for
//! now; replaced whole at reconfiguration later, so readers hold an `Arc`.

use messages::base::Digest;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::verify::{JWK, JwkId, Verifier};

pub struct EpochState {
    pub config: ProtocolConfig,
    pub chain: Chain,
    pub epoch: u64,
    /// The chain's genesis checkpoint digest.
    pub chain_identifier: Digest,
    pub reference_gas_price: u64,
    pub committee_size: u32,
    /// Verifies signatures under the epoch's protocol config and JWKs.
    pub verifier: Verifier,
}

impl EpochState {
    pub fn new(
        chain: Chain,
        protocol_version: u64,
        epoch: u64,
        chain_identifier: Digest,
        reference_gas_price: u64,
        committee_size: u32,
        jwks: impl IntoIterator<Item = (JwkId, JWK)>,
    ) -> EpochState {
        let config = ProtocolConfig::get_for_version(ProtocolVersion::new(protocol_version), chain);
        EpochState {
            verifier: Verifier::new(&config, chain, jwks),
            config,
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
