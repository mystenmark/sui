// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//
// Predicates and utility functions based on gas versions.
//
// Kept whole although the executor meters at gas model 15 or later: the
// unmetered `GasStatus` reports version 11, and code consulting these with
// its version must branch as the reference does.

use sui_protocol_config::ProtocolConfig;

use crate::gas_model::tables::{
    INITIAL_COST_SCHEDULE_V1, INITIAL_COST_SCHEDULE_V2, INITIAL_COST_SCHEDULE_V3,
    INITIAL_COST_SCHEDULE_V4, INITIAL_COST_SCHEDULE_V5,
};
use crate::gas_model::units_types::CostTable;

// Threshold after which native functions contribute to virtual instruction count.
const V2_NATIVE_FUNCTION_CALL_THRESHOLD: u64 = 700;

/// If true, do not charge the entire budget on storage OOG
pub fn dont_charge_budget_on_storage_oog(gas_model_version: u64) -> bool {
    gas_model_version >= 4
}

/// If true, enable the check for gas price too high
pub fn check_for_gas_price_too_high(gas_model_version: u64) -> bool {
    gas_model_version >= 4
}

/// If true, input object bytes are treated as memory allocated in Move and
/// charged according to the bucket they end up in.
pub fn charge_input_as_memory(gas_model_version: u64) -> bool {
    gas_model_version == 4
}

/// If true, calculate value sizes using the legacy size calculation.
pub fn use_legacy_abstract_size(gas_model_version: u64) -> bool {
    gas_model_version <= 7
}

// If true, use the value of txn_base_cost as a multiplier of transaction gas price
// to determine the minimum cost of a transaction.
pub fn txn_base_cost_as_multiplier(protocol_config: &ProtocolConfig) -> bool {
    protocol_config.txn_base_cost_as_multiplier()
}

// If true, charge differently for package upgrades
pub fn charge_upgrades(gas_model_version: u64) -> bool {
    gas_model_version >= 7
}

// Return the version supported cost table
pub fn cost_table_for_version(gas_model: u64) -> &'static CostTable {
    if gas_model <= 3 {
        &INITIAL_COST_SCHEDULE_V1
    } else if gas_model == 4 {
        &INITIAL_COST_SCHEDULE_V2
    } else if gas_model == 5 {
        &INITIAL_COST_SCHEDULE_V3
    } else if gas_model <= 7 {
        &INITIAL_COST_SCHEDULE_V4
    } else {
        &INITIAL_COST_SCHEDULE_V5
    }
}

// In gas model versions <= 13, charge_native_function_before_execution pops
// args that were already popped by charge_call - a double-pop. The resulting
// negative stack heights were masked by saturating_sub in pop_stack.
// Version 14+ fixes the double-pop so charge_native_function_before_execution
// no longer pops args.
pub fn legacy_charge_native_pops_args(gas_model_version: u64) -> bool {
    gas_model_version <= 13
}

// Return if the native function call threshold is exceeded
pub fn native_function_threshold_exceeded(gas_model_version: u64, num_native_calls: u64) -> bool {
    if gas_model_version > 8 {
        num_native_calls > V2_NATIVE_FUNCTION_CALL_THRESHOLD
    } else {
        false
    }
}

/// If true, re-read the gas payment location before final charging.
pub fn refresh_gas_payment_location(gas_model_version: u64) -> bool {
    gas_model_version >= 13
}

pub fn bump_only_enabled(gas_model_version: u64) -> bool {
    gas_model_version >= 15
}
