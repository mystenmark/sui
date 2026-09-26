// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions submitted over gRPC: decoded in the handler, validated and
//! their signatures verified on the processors, answered with
//! `validation::check`'s verdict.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use common::Case;
use tonic::Code;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validator::Validator;
use validator::epoch::EpochState;
use validator::processors::{Processors, ValidateTransactions};
use validator::proto::{RawSubmitTxRequest, SubmitTxType};
use validator::service::validator_client::ValidatorClient;
use workqueue::Queue;

fn cases() -> (Vec<Case>, Arc<EpochState>) {
    let (cases, jwks) = common::signed_vectors();
    (cases, common::vectors_epoch(4, jwks))
}

async fn serve(epoch: Arc<EpochState>) -> ValidatorClient<Channel> {
    let processors = Processors::start(&epoch, 64);
    let queue = processors.transactions.clone();
    serve_with(epoch, queue, processors).await
}

/// Serves with `queue`; `processors` (whatever drains it) live as long as
/// the server.
async fn serve_with(
    epoch: Arc<EpochState>,
    queue: Queue<ValidateTransactions>,
    processors: impl Send + 'static,
) -> ValidatorClient<Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let service = Validator::new(epoch, queue).into_service();
    tokio::spawn(async move {
        let _processors = processors;
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
    });
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    ValidatorClient::new(channel)
}

fn submit(cases: &[&Case], submit_type: SubmitTxType) -> RawSubmitTxRequest {
    RawSubmitTxRequest {
        transactions: cases.iter().map(|c| Bytes::from(c.bytes.clone())).collect(),
        submit_type: submit_type as i32,
    }
}

/// A case that passes, and one that fails.
fn valid_and_invalid<'c>(cases: &'c [Case], epoch: &EpochState) -> (&'c Case, &'c Case) {
    let valid = cases
        .iter()
        .find(|c| common::expected(epoch, &[c]).is_ok())
        .unwrap();
    let invalid = cases
        .iter()
        .find(|c| common::expected(epoch, &[c]).is_err())
        .unwrap();
    (valid, invalid)
}

#[tokio::test]
async fn verdicts_come_back_over_grpc() {
    let (cases, epoch) = cases();
    let mut client = serve(epoch.clone()).await;
    let (mut passed, mut failed) = (0, 0);
    for case in &cases {
        let status = client
            .submit_transaction(submit(&[case], SubmitTxType::Default))
            .await
            .unwrap_err();
        match common::expected(&epoch, &[case]) {
            Ok(()) => {
                assert_eq!(
                    status.code(),
                    Code::Unimplemented,
                    "{}: {status:?}",
                    case.label
                );
                passed += 1;
            }
            Err(kind) => {
                assert_eq!(
                    status.code(),
                    Code::InvalidArgument,
                    "{}: {status:?}",
                    case.label
                );
                assert!(
                    status.message().starts_with(&format!("{kind:?}:")),
                    "{}: expected {kind:?}, got {status:?}",
                    case.label
                );
                failed += 1;
            }
        }
    }
    assert!(
        passed > 10 && failed > 10,
        "{passed} passed, {failed} failed"
    );
}

#[tokio::test]
async fn malformed_requests_are_refused() {
    let (cases, epoch) = cases();
    let (valid, invalid) = valid_and_invalid(&cases, &epoch);
    let mut client = serve(epoch).await;
    let garbage = RawSubmitTxRequest {
        transactions: vec![Bytes::from_static(&[1, 2, 3])],
        submit_type: SubmitTxType::Default as i32,
    };
    let requests = [
        ("garbage", garbage),
        ("empty", submit(&[], SubmitTxType::Default)),
        (
            "ping with a transaction",
            submit(&[valid], SubmitTxType::Ping),
        ),
        (
            "unknown type",
            RawSubmitTxRequest {
                submit_type: 9,
                ..submit(&[valid], SubmitTxType::Default)
            },
        ),
        ("too many", submit(&[valid; 513], SubmitTxType::Default)),
        // One invalid transaction fails the whole request.
        (
            "one invalid",
            submit(&[valid, invalid], SubmitTxType::Default),
        ),
    ];
    for (label, request) in requests {
        let status = client.submit_transaction(request).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{label}: {status:?}");
    }
}

#[tokio::test]
async fn a_full_queue_refuses_work() {
    let (cases, epoch) = cases();
    let (valid, _) = valid_and_invalid(&cases, &epoch);
    // Nothing drains it: of two requests, one fills it and waits forever, and
    // the other finds it full.
    let (queue, inbox) = workqueue::queue(1);
    let client = serve_with(epoch, queue, inbox).await;
    let (mut a, mut b) = (client.clone(), client);
    let status = tokio::select! {
        r = a.submit_transaction(submit(&[valid], SubmitTxType::Default)) => r,
        r = b.submit_transaction(submit(&[valid], SubmitTxType::Default)) => r,
    }
    .unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
}

#[tokio::test]
async fn a_full_queue_between_processors_refuses_work() {
    let (cases, epoch) = cases();
    let (valid, _) = valid_and_invalid(&cases, &epoch);
    // Fails validation (a user cannot send a system transaction), before the
    // signature queue.
    let invalid = cases.iter().find(|c| c.label == "signed_system").unwrap();
    // A signature queue of no capacity, whose only reader is the validator's
    // own thread, never takes an item.
    let (queue, inbox) = workqueue::queue(16);
    let worker = Processors::worker(&epoch, inbox, 0).spawn();
    let mut client = serve_with(epoch, queue, worker).await;
    let status = client
        .submit_transaction(submit(&[valid], SubmitTxType::Default))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    let status = client
        .submit_transaction(submit(&[invalid], SubmitTxType::Default))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}
