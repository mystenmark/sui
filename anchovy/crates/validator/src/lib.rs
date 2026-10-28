// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's gRPC service, `sui.validator.Validator`.

pub mod checks;
pub mod codec;
pub mod consensus;
pub mod current;
pub mod epoch;
pub mod epoch_store;
pub mod pipeline;
pub mod processors;
pub mod proto;
pub mod randomness;
pub mod server;
pub mod submit;
pub mod tls;

pub use server::Validator;

/// tonic's generated client and server for the service.
#[allow(clippy::all, clippy::pedantic)]
pub mod service {
    include!(concat!(env!("OUT_DIR"), "/sui.validator.Validator.rs"));
}

/// Serves the API over TLS on `listener` until `shutdown` completes, over
/// whichever epoch `current` runs.
pub async fn serve(
    listener: tokio::net::TcpListener,
    key: &tls::NetworkKey,
    current: current::CurrentEpoch,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(Validator::over(current).into_service())
        .serve_with_incoming_shutdown(tls::incoming(listener, tls::server_config(key)), shutdown)
        .await
}
