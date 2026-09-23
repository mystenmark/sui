// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's gRPC service, `sui.validator.Validator`.

pub mod codec;
pub mod proto;
pub mod server;

pub use server::Validator;

/// tonic's generated client and server for the service.
#[allow(clippy::all, clippy::pedantic)]
pub mod service {
    include!(concat!(env!("OUT_DIR"), "/sui.validator.Validator.rs"));
}
