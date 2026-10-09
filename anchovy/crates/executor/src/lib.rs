// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui-execution`'s latest version, ported onto anchovy's zero-copy types:
//! the same modules, functions and order of checks, reading transactions
//! and objects as `messages` views, with per-transaction temporaries in an
//! arena. See `IMPLEMENTATION_PLAN_PHASE8.md`.

// Style-only lints that would reshape code ported line for line from the reference.
#![allow(
    clippy::doc_markdown,
    clippy::single_match_else,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

pub mod error;
pub mod gas;
pub mod gas_meter;
pub mod gas_model;
