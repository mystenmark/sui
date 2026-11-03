// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui-execution`'s latest version, ported onto anchovy's zero-copy types:
//! the same modules, functions and order of checks, reading transactions
//! and objects as `messages` views, with per-transaction temporaries in an
//! arena. See `IMPLEMENTATION_PLAN_PHASE8.md`.

// Pedantic style lints would reshape code ported line for line from the reference; the
// reference is held to sui's lints instead.
#![allow(clippy::pedantic)]

pub mod accumulator_event;
pub mod accumulator_root;
pub mod accumulator_settlement;
pub mod adapter;
pub mod data_store;
pub mod deny_list_v2;
pub mod effects;
pub mod error;
pub mod execution;
pub mod execution_engine;
pub mod execution_mode;
pub mod execution_params;
pub mod execution_value;
pub mod gas;
pub mod gas_charger;
pub mod gas_meter;
pub mod gas_model;
pub mod inputs;
pub mod layout_resolver;
pub mod move_package;
pub mod ptb_builder;
pub mod static_programmable_transactions;
pub mod storage;
pub mod temporary_store;
pub mod transaction;
pub mod type_layout_resolver;

/// Whether this executor runs `config`'s version: the reference's latest
/// execution version (4), from gas model 15 on.
pub fn runs(config: &sui_protocol_config::ProtocolConfig) -> bool {
    config.execution_version_as_option() == Some(4) && config.gas_model_version() >= 15
}
