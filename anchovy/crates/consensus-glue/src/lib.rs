// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! consensus-core (Mysticeti) under the node, as sui's `ConsensusManager`
//! runs it: only the translation between consensus-core and the pipeline,
//! and consensus-core's lifecycle. The voter and the commit handler are the
//! pipeline's (`validator::consensus`). Async code reaches the pipeline only
//! by pushing onto its queues an entry with a oneshot, and awaiting it.

mod commits;
mod committee;
mod config;
mod runner;
mod submit;
mod verifier;

pub use runner::Runner;
