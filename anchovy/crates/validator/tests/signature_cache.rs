// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The verified-signature cache saves work and never changes a verdict: a
//! resubmitted transaction is not verified again, and any other encoding,
//! even of the same transaction data, is judged as without the cache.

mod common;

use std::sync::Arc;

use common::{Case, Pipeline};
use messages::Message;
use messages::transaction::{DigestPending, Transaction};
use validation::ErrorKind;
use validator::epoch::EpochState;
use validator::signature_cache::GENERATION;

/// The contexts, each with its cases. A few vectors share their bytes
/// under two labels; only the first is kept, as the second would rightly
/// hit the first's entry.
fn contexts() -> Vec<(Arc<EpochState>, Vec<Case>)> {
    let (vectors, jwks) = common::signed_vectors();
    let (mainnet, epoch) = common::mainnet();
    let distinct = |cases: Vec<Case>| {
        let mut seen = std::collections::HashSet::new();
        cases
            .into_iter()
            .filter(|c| seen.insert(c.bytes.clone()))
            .collect::<Vec<_>>()
    };
    vec![
        (common::vectors_epoch(4, jwks), distinct(vectors)),
        (common::mainnet_epoch(epoch), distinct(mainnet)),
    ]
}

fn find<'c>(cases: &'c [Case], label: &str) -> &'c Case {
    cases.iter().find(|c| c.label == label).unwrap()
}

#[test]
fn resubmissions_hit_the_cache_with_the_same_verdicts() {
    for (epoch, cases) in contexts() {
        let mut pipeline = Pipeline::new(&epoch, GENERATION);
        let expected: Vec<_> = cases
            .iter()
            .map(|c| common::expected(&epoch, &[c]))
            .collect();
        for (i, case) in cases.iter().enumerate() {
            assert_eq!(pipeline.run(&[&case.bytes]), expected[i], "{}", case.label);
        }
        assert_eq!(pipeline.stats().0, 0);
        for (i, case) in cases.iter().enumerate() {
            assert_eq!(
                pipeline.run(&[&case.bytes]),
                expected[i],
                "{} again",
                case.label
            );
        }
        let valid = expected.iter().filter(|e| e.is_ok()).count() as u64;
        assert!(valid > 0);
        assert_eq!(pipeline.stats().0, valid, "one hit per valid transaction");
    }
}

/// Every single-bit change to a cached transaction, wherever it is (intent,
/// data, a signature, a length), gets the verdict it gets without a cache,
/// and never a cache hit.
#[test]
fn a_cached_transaction_changed_anywhere_is_judged_afresh() {
    let mut judged = 0;
    for (epoch, cases) in contexts() {
        for case in &cases {
            if common::expected(&epoch, &[case]).is_err() {
                continue;
            }
            // A cache of its own: some changed encodings are valid, and are
            // cached in turn.
            let mut pipeline = Pipeline::new(&epoch, GENERATION);
            pipeline.run(&[&case.bytes]).unwrap();
            let (hits, _) = pipeline.stats();
            for i in 0..case.bytes.len() {
                let mut bytes = case.bytes.clone();
                bytes[i] ^= 1;
                if Message::<Transaction<DigestPending>>::parse(bytes.clone()).is_err() {
                    continue;
                }
                let changed = Case {
                    label: format!("{} with byte {i} changed", case.label),
                    bytes,
                };
                let expected = common::expected(&epoch, &[&changed]);
                assert_eq!(
                    pipeline.run(&[&changed.bytes]),
                    expected,
                    "{}",
                    changed.label
                );
                judged += 1;
            }
            assert_eq!(
                pipeline.stats().0,
                hits,
                "{}: a changed encoding hit",
                case.label
            );
        }
    }
    assert!(judged > 1000, "{judged} changed transactions");
}

/// The vectors that share `verify_ed25519`'s data but not its signature.
#[test]
fn the_same_data_with_other_signatures_is_refused() {
    let (cases, jwks) = common::signed_vectors();
    let epoch = common::vectors_epoch(4, jwks);
    let mut pipeline = Pipeline::new(&epoch, GENERATION);
    pipeline
        .run(&[&find(&cases, "verify_ed25519").bytes])
        .unwrap();
    for label in [
        "verify_ed25519_flipped",
        "verify_ed25519_other_signer",
        "verify_ed25519_twice",
    ] {
        let case = find(&cases, label);
        let expected = common::expected(&epoch, &[case]);
        assert!(expected.is_err(), "{label}");
        assert_eq!(pipeline.run(&[&case.bytes]), expected, "{label}");
    }
    assert_eq!(pipeline.stats().0, 0);
}

#[test]
fn failures_are_not_cached() {
    let (cases, jwks) = common::signed_vectors();
    let epoch = common::vectors_epoch(4, jwks);
    let mut pipeline = Pipeline::new(&epoch, GENERATION);
    let case = find(&cases, "verify_ed25519_flipped");
    for round in 1..=3 {
        assert_eq!(
            pipeline.run(&[&case.bytes]),
            Err(ErrorKind::InvalidSignature)
        );
        assert_eq!(pipeline.stats(), (0, round));
    }
}
