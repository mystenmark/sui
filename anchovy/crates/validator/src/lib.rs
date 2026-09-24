// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's gRPC service, `sui.validator.Validator`.

pub mod codec;
pub mod epoch;
pub mod processors;
pub mod proto;
pub mod server;
pub mod tls;

pub use server::Validator;

/// tonic's generated client and server for the service.
#[allow(clippy::all, clippy::pedantic)]
pub mod service {
    include!(concat!(env!("OUT_DIR"), "/sui.validator.Validator.rs"));
}

/// Serves the API over TLS on `listener` until `shutdown` completes.
pub async fn serve(
    listener: tokio::net::TcpListener,
    key: &tls::NetworkKey,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(Validator::default().into_service())
        .serve_with_incoming_shutdown(tls::incoming(listener, tls::server_config(key)), shutdown)
        .await
}
