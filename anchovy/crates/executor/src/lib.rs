// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui-execution`'s latest version, ported onto anchovy's zero-copy types:
//! the same modules, functions and order of checks, reading transactions
//! and objects as `messages` views, with per-transaction temporaries in an
//! arena. See `IMPLEMENTATION_PLAN_PHASE8.md`.

// Pedantic style lints would reshape code ported line for line from the reference; the
// reference is held to sui's lints instead.
#![allow(clippy::pedantic, clippy::too_many_arguments)]

pub mod accumulator_event;
pub mod accumulator_root;
pub mod adapter;
pub mod data_store;
pub mod deny_list_v2;
pub mod effects;
pub mod error;
pub mod execution;
pub mod execution_mode;
pub mod execution_value;
pub mod gas;
pub mod gas_charger;
pub mod gas_meter;
pub mod gas_model;
pub mod inputs;
pub mod static_programmable_transactions;
pub mod storage;
pub mod temporary_store;
pub mod transaction;
