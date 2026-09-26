// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validation processor on its pool gives the verdicts of calling
//! `TransactionData::validity_check` directly, off the calling thread.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::base::Digest;
use messages::transaction::{DigestPending, DigestReady, Transaction};
use protocol_config::{Chain, ProtocolVersion};
use tokio::sync::oneshot;
use validator::epoch::EpochState;
use validator::processors::{TransactionValidator, ValidateTransactions};
use workqueue::Pool;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `TransactionData` bytes as an unsigned `Transaction`: one entry, the
/// transaction intent, the data, no signatures.
fn as_transaction(data: &[u8]) -> Vec<u8> {
    let mut out = vec![1, 0, 0, 0];
    out.extend_from_slice(data);
    out.push(0);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_verdicts_match_direct_calls() {
    let epoch = Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        5,
        Digest::new([0x11; 32]),
        1000,
        4,
    ));
    let make = {
        let epoch = epoch.clone();
        move || TransactionValidator::new(epoch.clone())
    };
    let (queue, _pool) = Pool::spawn("validate", 2, 1024, make);

    let mut compared = 0;
    for line in include_str!("../../validation/tests/data/validity.vectors").lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let ["tx", _, "tx_data", label, hex] = fields[..] else {
            continue;
        };
        let Ok(transaction) =
            Message::<Transaction<DigestPending>>::parse(as_transaction(&unhex(hex)))
        else {
            continue;
        };
        let bump = Bump::with_capacity(1 << 16);
        let direct = validation::transaction_data::validity_check(
            &transaction.get().0.data,
            &epoch.context(),
            &bump,
        )
        .map_err(|e| e.kind);

        let wire = transaction.wire_bytes().to_vec();
        let (reply, verdict) = oneshot::channel();
        queue
            .try_push(ValidateTransactions {
                transactions: vec![transaction],
                reply,
            })
            .unwrap_or_else(|_| panic!("queue refused"));
        let pooled = verdict.await.unwrap().map_err(|e| e.kind);
        match (pooled, direct) {
            (Ok(validated), Ok(())) => {
                // Hashed on the processor, as parsing with the digest would.
                let parsed = Message::<Transaction<DigestReady>>::parse(wire).unwrap();
                let [hashed] = &validated.0[..] else {
                    panic!("{label}: {} transactions back", validated.0.len())
                };
                assert_eq!(hashed.get(), parsed.get(), "{label}");
            }
            (pooled, direct) => assert_eq!(pooled.map(|_| ()), direct, "{label}"),
        }
        compared += 1;
    }
    assert!(compared > 100);
}
