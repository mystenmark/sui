// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The system state object, as an epoch is built from it.

use sui_types::sui_system_state::epoch_start_sui_system_state::EpochStartSystemStateTrait;
use sui_types::sui_system_state::{SuiSystemStateTrait, get_sui_system_state};

use crate::{Error, Result, StoreView};

/// What the system state object says an epoch is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemState {
    pub epoch: u64,
    pub protocol_version: u64,
    pub reference_gas_price: u64,
    pub epoch_start_timestamp_ms: u64,
    pub epoch_duration_ms: u64,
    /// Each active validator, in the committee's order: by protocol key.
    pub committee: Vec<Member>,
}

/// An active validator, as the epoch's committees take it: the
/// checkpoint committee its name and stake, consensus all of it, as sui's
/// `get_consensus_committee` builds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Its protocol (BLS) public key: the authority's name.
    pub name: [u8; 96],
    pub stake: u64,
    /// Consensus's key for its blocks (Ed25519): the validator's worker key.
    pub consensus_key: [u8; 32],
    /// Its network key (Ed25519), for TLS and as network identity.
    pub network_key: [u8; 32],
    /// Consensus's address (the primary address), a multiaddr.
    pub address: String,
    pub hostname: String,
}

impl SystemState {
    /// Each member's protocol key and stake: the checkpoint committee.
    pub fn stakes(&self) -> Vec<([u8; 96], u64)> {
        self.committee.iter().map(|m| (m.name, m.stake)).collect()
    }
}

/// The live system state object's view of its epoch.
///
/// # Errors
/// If the store cannot be read, or holds no system state.
pub fn read(store: &store::Store) -> Result<SystemState> {
    let state = get_sui_system_state(&StoreView::new(store)).map_err(Error::Sui)?;
    let (epoch, protocol_version, reference_gas_price, epoch_start_timestamp_ms, epoch_duration_ms) = (
        state.epoch(),
        state.protocol_version(),
        state.reference_gas_price(),
        state.epoch_start_timestamp_ms(),
        state.epoch_duration_ms(),
    );
    // Sorted by protocol key, as the checkpoint committee is.
    let committee = state
        .into_epoch_start_state()
        .get_consensus_committee()
        .authorities()
        .map(|(_, authority)| Member {
            name: authority
                .authority_name
                .to_bytes()
                .try_into()
                .expect("a 96-byte protocol key"),
            stake: authority.stake,
            consensus_key: authority
                .protocol_key
                .to_bytes()
                .try_into()
                .expect("a 32-byte Ed25519 key"),
            network_key: authority.network_key.to_bytes(),
            address: authority.address.to_string(),
            hostname: authority.hostname.clone(),
        })
        .collect();
    Ok(SystemState {
        epoch,
        protocol_version,
        reference_gas_price,
        epoch_start_timestamp_ms,
        epoch_duration_ms,
        committee,
    })
}
