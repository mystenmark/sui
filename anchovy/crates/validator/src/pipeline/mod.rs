// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The types the pipeline's processors exchange (`docs/consensus_pipeline.md`):
//! waiter keys, work items, the tables availability checks read, and the
//! interfaces to the black boxes (consensus, state sync). The processors live
//! beside their state: the built ones in this crate, the others in their own
//! crates (`resolver`, `exec-pool`, `settlement`, `checkpoint`,
//! `state-hasher`, `aggregator`), wired together by `node`.

pub mod items;
pub mod keys;
pub mod tables;
