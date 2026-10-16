// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The system state object, as an epoch is built from it.

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
    /// Each active validator's protocol key and voting power, in the
    /// committee's order.
    pub committee: Vec<([u8; 96], u64)>,
}

/// The live system state object's view of its epoch.
///
/// # Errors
/// If the store cannot be read, or holds no system state.
pub fn read(store: &store::Store) -> Result<SystemState> {
    let state = get_sui_system_state(&StoreView::new(store)).map_err(Error::Sui)?;
    let committee = state
        .get_current_epoch_committee()
        .committee()
        .voting_rights
        .iter()
        .map(|(name, power)| (name.0, *power))
        .collect();
    Ok(SystemState {
        epoch: state.epoch(),
        protocol_version: state.protocol_version(),
        reference_gas_price: state.reference_gas_price(),
        epoch_start_timestamp_ms: state.epoch_start_timestamp_ms(),
        epoch_duration_ms: state.epoch_duration_ms(),
        committee,
    })
}
