// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signed transactions and the epochs to check them in, shared by the
//! pipeline's tests: the validity vectors' signed cases (every signature
//! scheme, valid and not) and the checked-in mainnet checkpoint's
//! transactions. The expected verdict is `validation::check`'s, which is
//! differential-tested against the reference.

#![allow(dead_code)]

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::base::Digest;
use messages::checkpoint::CheckpointData;
use messages::transaction::{DigestPending, Transaction};
use protocol_config::{Chain, ProtocolVersion};
use validation::ErrorKind;
use validation::verify::{JWK, JwkId};
use validator::epoch::EpochState;

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A transaction as `SubmitTransaction` carries it.
pub struct Case {
    pub label: String,
    pub bytes: Vec<u8>,
}

impl Case {
    pub fn parse(&self) -> Option<Message<Transaction<'static, DigestPending>>> {
        Message::parse(self.bytes.clone()).ok()
    }
}

const VECTORS: &str = include_str!("../../../validation/tests/data/validity.vectors");

/// The vectors' `sender_signed` and `verify` cases that parse, and the JWKs
/// they were generated with.
pub fn signed_vectors() -> (Vec<Case>, Vec<(JwkId, JWK)>) {
    let mut cases = vec![];
    let mut jwks = vec![];
    for line in VECTORS.lines() {
        if let Some(json) = line.strip_prefix("jwks ") {
            jwks = serde_json::from_str(json).unwrap();
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        if let ["tx", _, "sender_signed" | "verify", label, hex] = fields[..] {
            let case = Case {
                label: label.to_owned(),
                bytes: unhex(hex),
            };
            if case.parse().is_some() {
                cases.push(case);
            }
        }
    }
    assert!(cases.len() > 50, "{} signed vectors", cases.len());
    (cases, jwks)
}

/// The context the vectors' signed cases were generated in.
pub fn vectors_epoch(epoch: u64, jwks: Vec<(JwkId, JWK)>) -> Arc<EpochState> {
    Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        epoch,
        Digest::new([0x11; 32]),
        1000,
        4,
        jwks,
    ))
}

/// The checked-in mainnet checkpoint's transactions, and its epoch.
pub fn mainnet() -> (Vec<Case>, u64) {
    let mut bytes = include_bytes!("../../../messages/tests/data/mainnet-325300367.chk").to_vec();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
    let view = checkpoint.get();
    let cases = view
        .transactions
        .iter()
        .enumerate()
        .map(|(i, tx)| Case {
            label: format!("mainnet {i}"),
            bytes: tx.transaction.bytes.to_vec(),
        })
        .collect();
    (cases, view.checkpoint_summary.data.epoch)
}

/// Mainnet's context, as the validation crate's corpus test fixes it. No
/// JWKs, so zkLogin fails.
pub fn mainnet_epoch(epoch: u64) -> Arc<EpochState> {
    Arc::new(EpochState::new(
        Chain::Mainnet,
        ProtocolVersion::MAX.as_u64(),
        epoch,
        Digest::new(
            unhex("35834a8ac17ca48fb14ac8f99c17c98747e95dd07294ae41a46b382246a4499b")
                .try_into()
                .unwrap(),
        ),
        1,
        100,
        [],
    ))
}

/// The verdict for a request of `cases`: the first failure of
/// `validation::check`, which checks a transaction's validity and then its
/// signatures, in order, as the reference does.
pub fn expected(epoch: &EpochState, cases: &[&Case]) -> Result<(), ErrorKind> {
    let context = epoch.context();
    for case in cases {
        let transaction = case.parse().unwrap();
        let bump = Bump::with_capacity(1 << 16);
        validation::check(&transaction.get().0, &context, &epoch.verifier, &[], &bump)
            .map_err(|e| e.kind)?;
    }
    Ok(())
}
