// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The signature index of each required signer, which a consensus
//! transaction's alias claim must match, against the reference's
//! `verify_sender_signed_data_message_signatures` over the validity vectors'
//! signed transactions. Compared where the reference's validity check passes
//! and it verifies the signatures, with no aliases.

mod common;

use std::sync::Arc;

use containers::Bump;
use sui_types::base_types::random_object_ref;
use sui_types::crypto::{AccountKeyPair, Signature, Signer, get_key_pair};
use sui_types::signature::VerifyParams;
use sui_types::signature_verification::{
    VerifiedDigestCache, verify_sender_signed_data_message_signatures,
};
use sui_types::transaction::{Transaction, TransactionData};
use validation::{sender_signed, verify};

/// Sponsored transfers signed with the sponsor's signature first and last,
/// by Ed25519 keys.
fn sponsored(epoch_gas_price: u64) -> Vec<common::Case> {
    let (sender, sender_key): (_, AccountKeyPair) = get_key_pair();
    let (sponsor, sponsor_key): (_, AccountKeyPair) = get_key_pair();
    let data = TransactionData::new_transfer_sui_allow_sponsor(
        sender,
        sender,
        Some(1),
        random_object_ref(),
        10_000_000,
        epoch_gas_price,
        sponsor,
    );
    let sender_key: &dyn Signer<Signature> = &sender_key;
    let sponsor_key: &dyn Signer<Signature> = &sponsor_key;
    [
        ("sender_first", vec![sender_key, sponsor_key]),
        ("sponsor_first", vec![sponsor_key, sender_key]),
    ]
    .into_iter()
    .map(|(label, signers)| common::Case {
        label: label.to_owned(),
        bytes: bcs::to_bytes(&Transaction::from_data_and_signer(data.clone(), signers)).unwrap(),
    })
    .collect()
}

#[test]
fn signer_indices_match_the_reference() {
    let (mut cases, jwks) = common::signed_vectors();
    let epoch = common::vectors_epoch(0, jwks);
    cases.extend(sponsored(epoch.reference_gas_price));
    // As the epoch's verifier is configured; zkLogin cases fail without its
    // providers and are not compared.
    let config = &epoch.config;
    let params = VerifyParams {
        verify_legacy_zklogin_address: config.verify_legacy_zklogin_address(),
        accept_zklogin_in_multisig: config.accept_zklogin_in_multisig(),
        accept_passkey_in_multisig: config.accept_passkey_in_multisig(),
        zklogin_max_epoch_upper_bound_delta: config.zklogin_max_epoch_upper_bound_delta(),
        additional_multisig_checks: config.additional_multisig_checks(),
        validate_zklogin_public_identifier: config.validate_zklogin_public_identifier(),
        ..VerifyParams::default()
    };
    let mut compared = 0;
    let mut mismatches = vec![];
    for case in &cases {
        // The reference verifies only what its validity check passed.
        let transaction = case.parse().unwrap();
        let bump = Bump::with_capacity(1 << 16);
        if sender_signed::validity_check(&transaction.get().0, &epoch.context(), &bump).is_err() {
            continue;
        }
        // Debug builds of the reference panic on some malformed multisigs
        // (`zip_debug_eq`), which release builds reject; those are skipped.
        let reference = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tx = bcs::from_bytes::<sui_types::transaction::Transaction>(&case.bytes).ok()?;
            verify_sender_signed_data_message_signatures(
                tx.data(),
                epoch.epoch,
                &params,
                Arc::new(VerifiedDigestCache::new_empty()),
                vec![],
            )
            .ok()
        }))
        .ok()
        .flatten();
        let Some(reference) = reference else {
            continue;
        };
        let signed = &transaction.get().0;
        let ours = sender_signed::deserialization_checks(signed, &bump).and_then(|(sigs, _)| {
            verify::signer_signature_indices(signed, sigs, &epoch.verifier, &[], &bump)
                .map(|indices| indices.to_vec())
        });
        compared += 1;
        if ours.as_ref().ok() != Some(&reference) {
            mismatches.push(format!(
                "{}: reference {reference:?}, ours {ours:?}",
                case.label
            ));
        }
    }
    assert!(compared >= 12, "only {compared} compared");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
