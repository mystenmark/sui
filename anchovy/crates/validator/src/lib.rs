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

/// Serves the API over TLS on `listener` until `shutdown` completes, with
/// `validation_threads` threads validating transactions. They stop once the
/// server has.
pub async fn serve(
    listener: tokio::net::TcpListener,
    key: &tls::NetworkKey,
    epoch: std::sync::Arc<epoch::EpochState>,
    validation_threads: usize,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let processors =
        processors::Processors::start(&epoch, validation_threads, processors::VALIDATION_QUEUE);
    let validator = Validator::new(epoch, processors.transactions.clone());
    let served = tonic::transport::Server::builder()
        .add_service(validator.into_service())
        .serve_with_incoming_shutdown(tls::incoming(listener, tls::server_config(key)), shutdown)
        .await;
    drop(processors);
    served
}
