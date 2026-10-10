// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Verifying signatures with their Ed25519 verification deferred to a
//! batch gives the verdict verifying them at once gives: for a valid
//! transaction, and for one whose Ed25519 key does not parse but whose
//! sender is the address its bytes hash to.

use blake2::Digest as _;
use containers::Bump;
use fastcrypto::ed25519::Ed25519PublicKey;
use fastcrypto::traits::ToFromBytes;
use messages::Message;
use messages::build;
use messages::transaction::{DigestPending, Transaction, TransactionKind};
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::sender_signed::deserialization_checks;
use validation::verify::{
    Verifier, verify_ed25519, verify_ed25519_batch, verify_signatures,
    verify_signatures_deferring_ed25519,
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A vector user transaction with one Ed25519 signature that verifies.
fn signed(verifier: &Verifier) -> Message<Transaction<'static, DigestPending>> {
    include_str!("data/validity.vectors")
        .lines()
        .filter_map(|line| match line.split(' ').collect::<Vec<_>>()[..] {
            ["tx", _, "sender_signed" | "verify", _, hex] => {
                Message::<Transaction<'static, DigestPending>>::parse(unhex(hex)).ok()
            }
            _ => None,
        })
        .find(|tx| {
            let signed = &tx.get().0;
            let bump = Bump::with_capacity(1 << 16);
            matches!(
                signed.data().kind(),
                TransactionKind::ProgrammableTransaction(_)
            ) && signed.tx_signatures().len() == 1
                && signed.tx_signatures()[0].0.first() == Some(&0)
                && deserialization_checks(signed, &bump).is_ok_and(|(sigs, _)| {
                    verify_signatures(signed, sigs, 0, verifier, &[], &bump).is_ok()
                })
        })
        .expect("a vector with one valid Ed25519 signature")
}

/// The verdicts at once and deferred, the deferred one as the batch gives
/// it and as each check alone does.
fn verdicts(
    transaction: &Message<Transaction<'static, DigestPending>>,
    verifier: &Verifier,
) -> (bool, bool, bool) {
    let signed = &transaction.get().0;
    let bump = Bump::with_capacity(1 << 16);
    let (sigs, _) = deserialization_checks(signed, &bump).unwrap();
    let at_once = verify_signatures(signed, sigs, 0, verifier, &[], &bump).is_ok();
    let mut deferred = Vec::new();
    let prepared =
        verify_signatures_deferring_ed25519(signed, sigs, 0, verifier, &[], &bump, &mut deferred)
            .is_ok();
    let batch = prepared && verify_ed25519_batch(&deferred);
    let each = prepared && deferred.iter().all(verify_ed25519);
    (at_once, batch, each)
}

#[test]
fn deferring_ed25519_verification_changes_no_verdict() {
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let verifier = Verifier::new(&config, Chain::Unknown, []);
    let valid = signed(&verifier);
    assert_eq!(verdicts(&valid, &verifier), (true, true, true));

    // A key that does not parse, signing for the address its bytes hash to.
    let key = (0..=u8::MAX)
        .map(|i| [i; 32])
        .find(|key| Ed25519PublicKey::from_bytes(key).is_err())
        .expect("some byte string is not a point");
    let address = build::base::SuiAddress(
        blake2::Blake2b::<blake2::digest::consts::U32>::digest([&[0][..], &key].concat()).into(),
    );
    let mut mirror = build::transaction::Transaction::from(&valid.get().0);
    let build::transaction::TransactionData::V1(data) = &mut mirror.data.0.intent_message.value;
    data.sender = address;
    data.gas_data.owner = address;
    let mut signature = mirror.data.0.tx_signatures[0].0.clone();
    signature[65..].copy_from_slice(&key);
    mirror.data.0.tx_signatures[0].0 = signature;
    let forged = Message::parse(bcs::to_bytes(&mirror).unwrap())
        .map_err(|(e, _)| e)
        .unwrap();
    assert_eq!(verdicts(&forged, &verifier), (false, false, false));
}
