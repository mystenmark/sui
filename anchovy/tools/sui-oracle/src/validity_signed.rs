// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validity vectors for `SenderSignedData`: deserialization (intent,
//! signature contents, what the reference checks as it decodes), scheme
//! gating, system transactions, and the size the reference counts. The
//! verdict of a passing case carries that size: `ok:<bytes>`.

use sui_types::transaction::{
    AllowedProposers, CallArg, FundsWithdrawalArg, GenesisTransaction, Reservation,
    SenderSignedData, TransactionData, TransactionExpiration, TransactionKind,
    TxValidityCheckContext, WithdrawFrom, WithdrawalTypeArg,
};

use crate::validity::{CHAIN_ID, EPOCH, Spec, chain_identifier, verdict_of};

/// `SenderSignedData` built by hand, so the intent and the signature bytes
/// can be anything.
fn signed(intent: [u8; 3], tx: &TransactionData, signatures: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&intent);
    out.extend(bcs::to_bytes(tx).unwrap());
    out.extend(bcs::to_bytes(&signatures.iter().map(|s| s.to_vec()).collect::<Vec<_>>()).unwrap());
    out
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A signature from the signature vectors, by label.
fn signature(label: &str) -> Vec<u8> {
    let vectors = crate::signatures::vectors();
    let line = vectors
        .lines()
        .find(|l| l.split(' ').next() == Some(label))
        .unwrap_or_else(|| panic!("no signature vector {label}"));
    unhex(line.split(' ').nth(1).unwrap())
}

/// Replaces the one occurrence of `from` in `bytes`.
fn patch(mut bytes: Vec<u8>, from: &[u8], to: &[u8]) -> Vec<u8> {
    let at = bytes
        .windows(from.len())
        .position(|w| w == from)
        .expect("pattern present");
    assert!(
        bytes[at + 1..].windows(from.len()).all(|w| w != from),
        "pattern unique"
    );
    bytes.splice(at..at + from.len(), to.iter().copied());
    bytes
}

pub(crate) fn cases() -> Vec<(String, Vec<u8>)> {
    let tx = Spec::new().build();
    let simple = signature("real_ed25519");
    let mut cases: Vec<(String, Vec<u8>)> = vec![];
    let mut add = |label: &str, bytes: Vec<u8>| cases.push((format!("signed_{label}"), bytes));

    add("one_signature", signed([0, 0, 0], &tx, &[&simple]));
    add("no_signatures", signed([0, 0, 0], &tx, &[]));
    add(
        "two_signatures",
        signed([0, 0, 0], &tx, &[&simple, &simple]),
    );
    for intent in [[1, 0, 0], [0, 1, 0], [0, 0, 1], [0, 0, 3]] {
        add(
            &format!("intent_{}{}{}", intent[0], intent[1], intent[2]),
            signed(intent, &tx, &[&simple]),
        );
    }
    for label in [
        "multisig_ok",
        "multisig_zklogin_parts",
        "legacy_ok",
        "legacy_bitmap_trailing",
        "legacy_empty_bitmap",
        "legacy_index_10",
        "simple_0_96",
        "empty",
        "passkey_ok",
        "passkey_json_create",
        "zklogin_vector_0",
        "zklogin_seed_over_modulus",
        "zklogin_header_hs256",
    ] {
        add(label, signed([0, 0, 0], &tx, &[&signature(label)]));
    }

    let genesis = Spec {
        kind: TransactionKind::Genesis(GenesisTransaction { objects: vec![] }),
        ..Spec::new()
    }
    .build();
    add("system", signed([0, 0, 0], &genesis, &[&simple]));

    // An empty proposer set cannot be built; patch a one-proposer set.
    let one_proposer = Spec {
        expiration: TransactionExpiration::Validity {
            min_epoch: Some(EPOCH),
            max_epoch: Some(EPOCH),
            min_timestamp: None,
            max_timestamp: None,
            chain: chain_identifier(CHAIN_ID),
            nonce: 9,
            allowed_proposers: Some(AllowedProposers {
                epoch: EPOCH,
                proposers: nonempty::nonempty![0x5eed_1234],
            }),
        },
        ..Spec::new()
    }
    .build();
    let bytes = signed([0, 0, 0], &one_proposer, &[&simple]);
    add("one_proposer", bytes.clone());
    add(
        "empty_proposers",
        patch(bytes, &[1, 0x34, 0x12, 0xed, 0x5e], &[0]),
    );

    // Identifiers in a withdrawal's type are checked while decoding: build
    // a valid one, then break it.
    let coin: sui_types::TypeTag = "0x2::abcd::ABCD".parse().unwrap();
    let withdraw = CallArg::FundsWithdrawal(FundsWithdrawalArg {
        reservation: Reservation::MaxAmountU64(5),
        type_arg: WithdrawalTypeArg::Balance(coin),
        withdraw_from: WithdrawFrom::Sender,
    });
    let bytes = signed(
        [0, 0, 0],
        &Spec::with_inputs(vec![withdraw]).build(),
        &[&simple],
    );
    add("withdraw_identifier_ok", bytes.clone());
    add(
        "withdraw_identifier_bad",
        patch(bytes, b"\x04abcd", b"\x04a-cd"),
    );

    // Gasless, for its size limit.
    let mut gasless = Spec::address_balance();
    gasless.price = 0;
    gasless.budget = 0;
    add("gasless", signed([0, 0, 0], &gasless.build(), &[&simple]));
    cases
}

/// The reference: decode, then `validity_check`, which returns the size.
pub(crate) fn verdict(bytes: &[u8], ctx: &TxValidityCheckContext<'_>) -> String {
    let Ok(tx) = bcs::from_bytes::<SenderSignedData>(bytes) else {
        return "TransactionDeserializationError".to_owned();
    };
    match tx.validity_check(ctx) {
        Ok(size) => format!("ok:{size}"),
        Err(e) => verdict_of(e),
    }
}
