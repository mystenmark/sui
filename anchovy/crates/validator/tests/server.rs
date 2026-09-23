// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Every route of an in-process server, through tonic's generated client.

use std::net::SocketAddr;

use bytes::Bytes;
use messages::base::{Digest, ObjectId};
use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, ObjectInfoRequestKind,
    SystemStateRequest, TransactionInfoRequest,
};
use tonic::Code;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validator::Validator;
use validator::codec::{BcsCodec, Encoded};
use validator::proto::{
    PingType, RawSubmitTxRequest, RawValidatorHealthRequest, RawValidatorHealthResponse,
    RawWaitForEffectsRequest, SubmitTxType,
};
use validator::service::validator_client::ValidatorClient;

async fn serve() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(Validator::default().into_service())
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    addr
}

async fn channel(addr: SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

async fn assert_healthy(client: &mut ValidatorClient<Channel>) {
    let health = client
        .validator_health(RawValidatorHealthRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health, RawValidatorHealthResponse::default());
}

/// A request that decoded and reached a `todo!()` handler, whose panic
/// resets the stream.
fn assert_reached_todo<T: std::fmt::Debug>(result: Result<T, tonic::Status>) {
    let status = result.unwrap_err();
    assert!(
        !matches!(status.code(), Code::InvalidArgument | Code::Unimplemented),
        "{status:?}"
    );
}

#[tokio::test]
async fn every_route_is_served() {
    let addr = serve().await;
    let mut client = ValidatorClient::new(channel(addr).await);
    assert_healthy(&mut client).await;

    assert_reached_todo(
        client
            .submit_transaction(RawSubmitTxRequest {
                transactions: vec![],
                submit_type: SubmitTxType::Ping as i32,
            })
            .await,
    );
    assert_reached_todo(
        client
            .wait_for_effects(RawWaitForEffectsRequest {
                transaction_digest: None,
                consensus_position: Some(Bytes::from_static(&[0; 8])),
                include_details: false,
                ping_type: Some(PingType::Consensus as i32),
            })
            .await,
    );
    assert_reached_todo(
        client
            .object_info(ObjectInfoRequest {
                object_id: ObjectId([1; 32]),
                generate_layout: false,
                request_kind: ObjectInfoRequestKind::LatestObjectInfo,
            })
            .await,
    );
    assert_reached_todo(
        client
            .transaction_info(TransactionInfoRequest {
                transaction_digest: Digest::new([2; 32]),
            })
            .await,
    );
    assert_reached_todo(
        client
            .checkpoint(CheckpointRequest {
                sequence_number: None,
                request_content: true,
            })
            .await,
    );
    assert_reached_todo(
        client
            .checkpoint_v2(CheckpointRequestV2 {
                sequence_number: Some(7),
                request_content: false,
                certified: true,
            })
            .await,
    );
    assert_reached_todo(
        client
            .get_system_state_object(SystemStateRequest { unused: false })
            .await,
    );

    // The panics took down their streams only.
    assert_healthy(&mut client).await;
}

#[tokio::test]
async fn malformed_bcs_is_rejected_before_the_handler() {
    let addr = serve().await;
    let mut grpc = tonic::client::Grpc::new(channel(addr).await);
    grpc.ready().await.unwrap();
    let status = grpc
        .unary::<Encoded, Encoded, _>(
            tonic::Request::new(Encoded(Bytes::from_static(&[1, 2, 3]))),
            "/sui.validator.Validator/Checkpoint".parse().unwrap(),
            BcsCodec::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}
