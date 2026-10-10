// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui-types` execution uses, on anchovy's views: shared by
//! the natives and the executor, as `sui-types` is by theirs.

// Style-only lints that would reshape code ported line for line from the reference.
#![allow(
    clippy::doc_markdown,
    clippy::single_match_else,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

pub mod base;
pub mod error;
pub mod execution;
pub mod object;
pub mod storage;
pub mod tx_context;
pub mod type_tags;
