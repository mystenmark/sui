// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions submitted over gRPC: decoded in the handler, validated on
//! the processor pool, answered with the validity-check verdict of the
//! reference's vectors.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use messages::base::Digest;
use protocol_config::{Chain, ProtocolVersion};
use tonic::Code;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validator::Validator;
use validator::epoch::EpochState;
use validator::processors::{Processors, TransactionValidator, ValidateTransactions};
use validator::proto::{RawSubmitTxRequest, SubmitTxType};
use validator::service::validator_client::ValidatorClient;
use workqueue::{Pool, Queue};

/// The context of the validity vectors' first case set.
fn epoch() -> Arc<EpochState> {
    Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        5,
        Digest::new([0x11; 32]),
        1000,
        4,
    ))
}

async fn serve() -> ValidatorClient<Channel> {
    let epoch = epoch();
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

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `TransactionData` bytes as an unsigned `Transaction`.
fn as_transaction(data: &[u8]) -> Bytes {
    let mut out = vec![1, 0, 0, 0];
    out.extend_from_slice(data);
    out.push(0);
    out.into()
}

fn submit(transactions: Vec<Bytes>, submit_type: SubmitTxType) -> RawSubmitTxRequest {
    RawSubmitTxRequest {
        transactions,
        submit_type: submit_type as i32,
    }
}

/// The verdict of each `tx_data` vector in the epoch's context, from its
/// case lines.
fn vectors() -> Vec<(String, Bytes, String)> {
    let max = ProtocolVersion::MAX.as_u64();
    let mut txs = std::collections::HashMap::new();
    let mut out = vec![];
    for line in include_str!("../../validation/tests/data/validity.vectors").lines() {
        let f: Vec<&str> = line.split(' ').collect();
        match f[..] {
            ["tx", id, "tx_data", label, hex] => {
                txs.insert(id, (label.to_owned(), as_transaction(&unhex(hex))));
            }
            [
                "case",
                id,
                "Unknown",
                versions,
                "5",
                chain,
                "1000",
                "4",
                verdict,
            ] if chain == "11".repeat(32) => {
                let (first, last) = versions.split_once('-').unwrap();
                let covers = (first.parse::<u64>().unwrap()..=last.parse().unwrap()).contains(&max);
                if let (true, Some((label, bytes))) = (covers, txs.get(id)) {
                    out.push((label.clone(), bytes.clone(), verdict.to_owned()));
                }
            }
            _ => {}
        }
    }
    out
}

#[tokio::test]
async fn verdicts_come_back_over_grpc() {
    let mut client = serve().await;
    let cases = vectors();
    assert!(cases.len() > 100);
    for (label, bytes, verdict) in cases {
        let status = client
            .submit_transaction(submit(vec![bytes], SubmitTxType::Default))
            .await
            .unwrap_err();
        if verdict == "ok" {
            assert_eq!(status.code(), Code::Unimplemented, "{label}: {status:?}");
        } else {
            assert_eq!(status.code(), Code::InvalidArgument, "{label}: {status:?}");
            assert!(
                status.message().starts_with(&format!("{verdict}:")),
                "{label}: expected {verdict}, got {status:?}"
            );
        }
    }
}

#[tokio::test]
async fn malformed_requests_are_refused() {
    let mut client = serve().await;
    let valid = vectors().into_iter().find(|(_, _, v)| v == "ok").unwrap().1;
    let cases = [
        (
            "garbage",
            submit(vec![Bytes::from_static(&[1, 2, 3])], SubmitTxType::Default),
        ),
        ("empty", submit(vec![], SubmitTxType::Default)),
        (
            "ping with a transaction",
            submit(vec![valid.clone()], SubmitTxType::Ping),
        ),
        (
            "unknown type",
            RawSubmitTxRequest {
                transactions: vec![valid.clone()],
                submit_type: 9,
            },
        ),
        (
            "too many",
            submit(vec![valid.clone(); 513], SubmitTxType::Default),
        ),
    ];
    for (label, request) in cases {
        let status = client.submit_transaction(request).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{label}: {status:?}");
    }
    // One invalid transaction fails the whole request.
    let invalid = vectors().into_iter().find(|(_, _, v)| v != "ok").unwrap().1;
    let status = client
        .submit_transaction(submit(vec![valid, invalid], SubmitTxType::Default))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

#[tokio::test]
async fn a_full_queue_refuses_work() {
    // No threads drain it: of two requests, one fills it and waits
    // forever, and the other finds it full.
    let make = || -> TransactionValidator { unreachable!() };
    let (queue, pool) = Pool::spawn("validate", 0, 1, make);
    let client = serve_with(epoch(), queue, pool).await;
    let valid = vectors().into_iter().find(|(_, _, v)| v == "ok").unwrap().1;
    let (mut a, mut b) = (client.clone(), client);
    let status = tokio::select! {
        r = a.submit_transaction(submit(vec![valid.clone()], SubmitTxType::Default)) => r,
        r = b.submit_transaction(submit(vec![valid], SubmitTxType::Default)) => r,
    }
    .unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
}
