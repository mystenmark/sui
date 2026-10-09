// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::gas`. Only the gas model from version 15 on exists here, so
//! `SuiGasStatus` is that model's status rather than an enum over versions.

use messages::base::ObjectId;
use messages::effects::GasCostSummary;
use sui_protocol_config::ProtocolConfig;

use crate::error::{ExecutionError, UserInputError};
use crate::gas_model::gas_predicates::check_for_gas_price_too_high;
pub use crate::gas_model::gas_v3::SuiGasStatus;
use crate::gas_model::tables::GasStatus;

pub trait SuiGasStatusAPI {
    fn is_unmetered(&self) -> bool;
    fn move_gas_status(&self) -> &GasStatus;
    fn move_gas_status_mut(&mut self) -> &mut GasStatus;
    fn bucketize_computation(
        &mut self,
        aborted: Option<bool>,
    ) -> Result<(), ExecutionError<'static>>;
    fn summary(&self) -> GasCostSummary;
    fn gas_budget(&self) -> u64;
    fn gas_price(&self) -> u64;
    fn reference_gas_price(&self) -> u64;
    fn storage_gas_units(&self) -> u64;
    fn storage_rebate(&self) -> u64;
    fn unmetered_storage_rebate(&self) -> u64;
    fn gas_used(&self) -> u64;
    fn reset_storage_cost_and_rebate(&mut self);
    fn charge_storage_read(&mut self, size: usize) -> Result<(), ExecutionError<'static>>;
    fn charge_publish_package(&mut self, size: usize) -> Result<(), ExecutionError<'static>>;
    fn track_storage_mutation(
        &mut self,
        object_id: &ObjectId,
        new_size: usize,
        storage_rebate: u64,
    ) -> Option<u64>;
    fn charge_storage_and_rebate(&mut self) -> Result<(), ExecutionError<'static>>;
    fn adjust_computation_on_out_of_gas(&mut self);
}

impl SuiGasStatus {
    /// # Panics
    /// Below gas model 15, which this executor does not implement.
    pub fn new(
        gas_budget: u64,
        gas_price: u64,
        reference_gas_price: u64,
        config: &ProtocolConfig,
    ) -> Result<Self, UserInputError> {
        // Common checks. We may pull them into version specific status as needed, but they
        // are unlikely to change.

        // gas price must be bigger or equal to reference gas price
        if gas_price < reference_gas_price {
            return Err(UserInputError::GasPriceUnderRGP {
                gas_price,
                reference_gas_price,
            });
        }
        if check_for_gas_price_too_high(config.gas_model_version())
            && gas_price >= config.max_gas_price()
        {
            return Err(UserInputError::GasPriceTooHigh {
                max_gas_price: config.max_gas_price(),
            });
        }

        assert!(
            config.gas_model_version() >= 15,
            "gas model {} predates this executor",
            config.gas_model_version()
        );
        Ok(Self::new_with_budget(
            gas_budget,
            gas_price,
            reference_gas_price,
            config,
        ))
    }
}
