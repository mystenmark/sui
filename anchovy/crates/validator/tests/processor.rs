// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Requests through the processors (validation, then signature verification,
//! on one worker thread) get `validation::check`'s verdict for their first
//! failing transaction, and passing transactions come back with the
//! digests parsing would have computed.

mod common;

use std::sync::Arc;

use common::Case;
use messages::Message;
use messages::transaction::{DigestReady, Transaction};
use tokio::sync::oneshot;
use validation::ErrorKind;
use validator::epoch::EpochState;
use validator::processors::{Processors, Rejected, Request, Validated};

async fn submit(
    processors: &Processors,
    epoch: &Arc<EpochState>,
    cases: &[&Case],
) -> Result<Validated, Rejected> {
    let (reply, verdict) = oneshot::channel();
    let transactions = cases.iter().map(|c| c.parse().unwrap()).collect();
    processors
        .transactions
        .try_push(Request::new(epoch.clone(), transactions, reply))
        .unwrap_or_else(|_| panic!("queue refused"));
    verdict.await.unwrap()
}

/// Sends each request and checks its verdict; returns how many passed and
/// how many failed.
async fn compare(epoch: &Arc<EpochState>, requests: &[Vec<&Case>]) -> (usize, usize) {
    let processors = Processors::start(1024);
    let (mut passed, mut failed) = (0, 0);
    for request in requests {
        let labels: Vec<&str> = request.iter().map(|c| c.label.as_str()).collect();
        let expected = common::expected(epoch, request);
        match (submit(&processors, epoch, request).await, expected) {
            (Ok(Validated(transactions)), Ok(())) => {
                assert_eq!(transactions.len(), request.len(), "{labels:?}");
                for (i, hashed) in transactions.iter().enumerate() {
                    let expected =
                        Message::<Transaction<DigestReady>>::parse(request[i].bytes.clone())
                            .unwrap();
                    let (hashed, expected) = (&hashed.get().0, &expected.get().0);
                    assert_eq!(hashed.digest(), expected.digest(), "{labels:?}");
                    assert_eq!(hashed.bytes(), expected.bytes(), "{labels:?}");
                }
                passed += 1;
            }
            (Err(Rejected::Invalid(e)), Err(kind)) => {
                assert_eq!(e.kind, kind, "{labels:?}: {e:?}");
                failed += 1;
            }
            (ours, expected) => panic!(
                "{labels:?}: ours {:?}, expected {expected:?}",
                ours.map(|_| ())
            ),
        }
    }
    (passed, failed)
}

/// Single requests, neighbouring pairs in each order, and triples.
fn requests(cases: &[Case]) -> Vec<Vec<&Case>> {
    let mut requests: Vec<Vec<&Case>> = cases.iter().map(|c| vec![c]).collect();
    for pair in cases.windows(2) {
        requests.push(vec![&pair[0], &pair[1]]);
        requests.push(vec![&pair[1], &pair[0]]);
    }
    for triple in cases.windows(3) {
        requests.push(triple.iter().collect());
    }
    requests
}

#[tokio::test]
async fn signed_vectors_match_validation_check() {
    let (cases, jwks) = common::signed_vectors();
    // The `verify` cases were generated in epoch 4, the `sender_signed` ones
    // in epoch 5.
    for epoch in [4, 5] {
        let epoch = common::vectors_epoch(epoch, jwks.clone());
        let (passed, failed) = compare(&epoch, &requests(&cases)).await;
        assert!(
            passed > 20 && failed > 20,
            "{passed} passed, {failed} failed"
        );
    }
}

#[tokio::test]
async fn mainnet_transactions_match_validation_check() {
    let (cases, epoch) = common::mainnet();
    let epoch = common::mainnet_epoch(epoch);
    let (passed, _) = compare(&epoch, &requests(&cases)).await;
    assert!(passed > 0);
}

/// A request whose first transaction's signature is bad and whose second is
/// invalid fails for the signature: the reference checks each transaction's
/// signatures before validating the next.
#[tokio::test]
async fn a_bad_signature_comes_before_a_later_invalid_transaction() {
    let (cases, jwks) = common::signed_vectors();
    let epoch = common::vectors_epoch(4, jwks);
    let find = |label: &str| cases.iter().find(|c| c.label == label).unwrap();
    let (bad_signature, invalid) = (find("verify_ed25519_flipped"), find("signed_system"));
    assert_eq!(
        common::expected(&epoch, &[bad_signature]),
        Err(ErrorKind::InvalidSignature)
    );
    assert!(common::expected(&epoch, &[invalid]).is_err_and(|k| k != ErrorKind::InvalidSignature));
    let processors = Processors::start(16);
    match submit(&processors, &epoch, &[bad_signature, invalid]).await {
        Err(Rejected::Invalid(e)) => assert_eq!(e.kind, ErrorKind::InvalidSignature),
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// A request is checked in the epoch it carries: of two requests through
/// the same processors, only the one whose epoch has the JWKs a zkLogin
/// signature needs passes.
#[tokio::test]
async fn a_request_is_checked_in_its_own_epoch() {
    let (cases, jwks) = common::signed_vectors();
    let zklogin = cases.iter().find(|c| c.label == "verify_zklogin").unwrap();
    let with_jwks = common::vectors_epoch(4, jwks);
    let without = common::vectors_epoch(4, vec![]);
    assert!(common::expected(&with_jwks, &[zklogin]).is_ok());
    assert!(common::expected(&without, &[zklogin]).is_err());
    let processors = Processors::start(16);
    assert!(submit(&processors, &with_jwks, &[zklogin]).await.is_ok());
    assert!(submit(&processors, &without, &[zklogin]).await.is_err());
    assert!(submit(&processors, &with_jwks, &[zklogin]).await.is_ok());
}
