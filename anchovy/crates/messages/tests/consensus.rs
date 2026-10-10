// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ConsensusTransaction` against sui-types' decoder, with the vectors
//! `sui-oracle --consensus-vectors` writes.

use std::io::Read as _;

use messages::Message;
use messages::build;
use messages::consensus::{ConsensusTransaction, ConsensusTransactionKind};
use messages::transaction::{DigestPending, Transaction};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn vectors() -> Vec<(String, bool, Vec<u8>)> {
    let mut text = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("data/consensus.vectors.gz")[..])
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .map(|line| {
            let [label, verdict, hex] = line.split(' ').collect::<Vec<_>>()[..] else {
                panic!("bad line {line}");
            };
            (label.to_string(), verdict == "ok", unhex(hex))
        })
        .collect()
}

/// Parses exactly what sui-types decodes, except where sui-types rejects
/// something parsing leaves to validation; re-encodes what it parses.
#[test]
fn matches_reference() {
    let vectors = vectors();
    let mut kinds = [false; 14];
    for (label, sui_ok, bytes) in &vectors {
        let parsed = Message::<ConsensusTransaction>::parse(bytes.clone());
        let semantic = label.starts_with("semantic_");
        assert_eq!(
            parsed.is_ok(),
            *sui_ok || semantic,
            "{label}: {:?}",
            parsed.as_ref().err().map(|(e, _)| e)
        );
        assert!(!semantic || !sui_ok, "{label}");
        let Ok(message) = parsed else { continue };
        let view = message.get();

        let mirror = build::consensus::ConsensusTransaction::from(view);
        assert_eq!(&bcs::to_bytes(&mirror).unwrap(), bytes, "{label}");
        assert_eq!(
            bcs::from_bytes::<build::consensus::ConsensusTransaction>(bytes).unwrap(),
            mirror,
            "{label}"
        );
        kinds[bytes[8] as usize] = true;

        if let ConsensusTransactionKind::UserTransactionV2(v2) = view.kind() {
            let span = v2.transaction_bytes();
            let tx = Message::<Transaction<'static, DigestPending>>::parse(span.to_vec())
                .unwrap_or_else(|(e, _)| panic!("{label}: {e}"));
            assert_eq!(tx.get(), v2.transaction(), "{label}");
            let mirror = build::transaction::Transaction::from(&tx.get().0);
            assert_eq!(bcs::to_bytes(&mirror).unwrap(), span, "{label}");
            let at = 9;
            assert_eq!(&bytes[at..at + span.len()], span, "{label}");
        }
    }
    assert_eq!(kinds, [true; 14], "every kind has an accepted vector");
}

#[test]
fn accessors() {
    for (label, sui_ok, bytes) in vectors() {
        if !sui_ok {
            continue;
        }
        let message = Message::<ConsensusTransaction>::parse(bytes.clone()).unwrap();
        let view = message.get();
        assert_eq!(view.tracking_id(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        let kind = view.kind();
        let has_authority = !matches!(
            kind,
            ConsensusTransactionKind::CertifiedTransaction(_)
                | ConsensusTransactionKind::RandomnessStateUpdate { .. }
                | ConsensusTransactionKind::UserTransaction(_)
                | ConsensusTransactionKind::UserTransactionV2(_)
        );
        assert_eq!(kind.authority().is_some(), has_authority, "{label}");
        match (label.as_str(), kind) {
            ("dkg_message", ConsensusTransactionKind::RandomnessDkgMessage(dkg)) => {
                assert_eq!(dkg.byte_len(), 200);
            }
            ("dkg_confirmation", ConsensusTransactionKind::RandomnessDkgConfirmation(dkg)) => {
                assert_eq!(dkg.byte_len(), 33);
            }
            (
                "execution_time_observation",
                ConsensusTransactionKind::ExecutionTimeObservation(observation),
            ) => assert_eq!(observation.estimate_count(), 7),
            ("user_transaction_v2_all_claims", ConsensusTransactionKind::UserTransactionV2(v2)) => {
                assert_eq!(v2.claims().len(), 3);
            }
            _ => {}
        }
    }
}
