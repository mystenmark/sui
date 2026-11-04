// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Every route of an in-process server, through tonic's generated client.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use messages::base::{Digest, ObjectId};
use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, ObjectInfoRequestKind,
    SystemStateRequest, TransactionInfoRequest,
};
use sui_protocol_config::{Chain, ProtocolVersion};
use tonic::Code;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validator::Validator;
use validator::codec::{BcsCodec, Encoded};
use validator::epoch::EpochState;
use validator::proto::{
    PingType, RawValidatorHealthRequest, RawValidatorHealthResponse, RawWaitForEffectsRequest,
};
use validator::service::validator_client::ValidatorClient;

async fn serve() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let epoch = Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        5,
        Digest::new([0x11; 32]),
        1000,
        4,
        [],
    ));
    // No genesis: these requests read no objects.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let (transactions, worker) = common::submission(&epoch, &store, 64);
    let (current, consensus) = common::current(&epoch, transactions, &store);
    let service = Validator::over(current).into_service();
    tokio::spawn(async move {
        // The processors, consensus and store live as long as the server.
        let _processors = (worker, consensus, dir);
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
    });
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

/// A request that decoded and reached a handler not yet ported.
fn assert_unimplemented<T: std::fmt::Debug>(result: Result<T, tonic::Status>) {
    assert_eq!(result.unwrap_err().code(), Code::Unimplemented);
}

#[tokio::test]
async fn every_route_is_served() {
    let addr = serve().await;
    let mut client = ValidatorClient::new(channel(addr).await);
    assert_healthy(&mut client).await;

    let status = client
        .wait_for_effects(RawWaitForEffectsRequest {
            transaction_digest: None,
            consensus_position: Some(Bytes::from_static(&[0; 8])),
            include_details: false,
            ping_type: Some(PingType::Consensus as i32),
        })
        .await
        .unwrap_err();
    assert_eq!(
        common::reference_error_name(&sui_types::error::SuiError::from(status)),
        "GrpcMessageDeserializeError"
    );
    assert_unimplemented(
        client
            .object_info(ObjectInfoRequest {
                object_id: ObjectId([1; 32]),
                generate_layout: false,
                request_kind: ObjectInfoRequestKind::LatestObjectInfo,
            })
            .await,
    );
    assert_unimplemented(
        client
            .transaction_info(TransactionInfoRequest {
                transaction_digest: Digest::new([2; 32]),
            })
            .await,
    );
    assert_unimplemented(
        client
            .checkpoint(CheckpointRequest {
                sequence_number: None,
                request_content: true,
            })
            .await,
    );
    assert_unimplemented(
        client
            .checkpoint_v2(CheckpointRequestV2 {
                sequence_number: Some(7),
                request_content: false,
                certified: true,
            })
            .await,
    );
    assert_unimplemented(
        client
            .get_system_state_object(SystemStateRequest { unused: false })
            .await,
    );

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

/// A wait as the reference's client encodes it is answered, and one it
/// cannot decode or that lacks what it needs fails as the reference's does.
#[tokio::test]
async fn waits_are_decoded_as_the_reference_s_client_encodes_them() {
    use sui_types::digests::TransactionDigest;
    use sui_types::messages_consensus::ConsensusPosition;
    use sui_types::messages_grpc::{PingType as ReferencePing, WaitForEffectsRequest};

    let addr = serve().await;
    let mut client = ValidatorClient::new(channel(addr).await);
    let reference = |transaction_digest, ping_type, epoch| {
        let raw =
            sui_types::messages_grpc::RawWaitForEffectsRequest::try_from(WaitForEffectsRequest {
                transaction_digest,
                // The reference's position, from anchovy's encoding of it.
                consensus_position: Some(
                    bcs::from_bytes::<ConsensusPosition>(
                        &validator::consensus::status::ConsensusPosition {
                            epoch,
                            block: ::consensus::BlockRef {
                                round: 1,
                                author: 0,
                                digest: [0; 32],
                            },
                            index: 0,
                        }
                        .to_bcs(),
                    )
                    .unwrap(),
                ),
                include_details: false,
                ping_type,
            })
            .unwrap();
        RawWaitForEffectsRequest {
            transaction_digest: raw.transaction_digest,
            consensus_position: raw.consensus_position,
            include_details: raw.include_details,
            ping_type: raw.ping_type,
        }
    };
    let error = |status: tonic::Status| {
        let error = sui_types::error::SuiError::from(status);
        (common::reference_error_name(&error), error.to_string())
    };

    // A position in an epoch past is answered as the epoch's end, for a
    // transaction and for a ping alike.
    let digest = Some(TransactionDigest::new([7; 32]));
    for ping in [None, Some(ReferencePing::Consensus)] {
        let status = client
            .wait_for_effects(reference(digest, ping, 4))
            .await
            .unwrap_err();
        assert_eq!(status.message(), "EpochEnded", "{status:?}");
    }

    let mut truncated = reference(digest, None, 5);
    truncated.transaction_digest = Some(Bytes::from_static(&[32; 32]));
    let mut no_position = reference(None, Some(ReferencePing::Consensus), 5);
    no_position.consensus_position = None;
    let mut unknown_ping = reference(None, None, 5);
    unknown_ping.ping_type = Some(99);
    for (request, expected) in [
        (
            truncated,
            ("GrpcMessageDeserializeError", "unexpected end of input"),
        ),
        (
            reference(None, None, 5),
            (
                "InvalidRequest",
                "Transaction digest is required for wait for effects requests",
            ),
        ),
        (
            no_position,
            (
                "InvalidRequest",
                "Consensus position is required for Ping requests",
            ),
        ),
        (unknown_ping, ("GrpcMessageDeserializeError", "ping_type")),
    ] {
        let (name, text) = error(client.wait_for_effects(request).await.unwrap_err());
        assert_eq!(name, expected.0);
        assert!(text.contains(expected.1), "{text}");
    }
}
