// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Real mainnet transactions against the reference's verdicts, from
//! `sui-oracle --validity-corpus`: the checked-in checkpoint always, and
//! the fetched corpus (`corpus/mainnet`) when present.

use std::path::{Path, PathBuf};

use containers::Bump;
use messages::Message;
use messages::base::Digest;
use messages::checkpoint::CheckpointData;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::Context;
use validation::verify::Verifier;

/// Mainnet's chain identifier; the rest of the context as the oracle's
/// `validity_corpus.rs` fixes it.
const MAINNET_CHAIN_ID: &str = "35834a8ac17ca48fb14ac8f99c17c98747e95dd07294ae41a46b382246a4499b";
const RGP: u64 = 1;
const COMMITTEE_SIZE: u32 = 100;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Checks one checkpoint; returns how many transactions it held.
fn check(chk: &Path, verdicts: &Path, mismatches: &mut Vec<String>) -> usize {
    let mut bytes = std::fs::read(chk).unwrap();
    assert_eq!(bytes.remove(0), 1, "{}: not a BCS blob", chk.display());
    let checkpoint = Message::<CheckpointData>::parse(bytes)
        .map_err(|(e, _)| e)
        .unwrap();
    let checkpoint = checkpoint.get();
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Mainnet);
    let epoch = checkpoint.checkpoint_summary.data.epoch;
    let ctx = Context {
        config: &config,
        epoch,
        chain_identifier: Digest::new(unhex(MAINNET_CHAIN_ID).try_into().unwrap()),
        reference_gas_price: RGP,
        committee_size: COMMITTEE_SIZE,
    };
    let verifier = Verifier::new(&config, Chain::Mainnet, []);

    let expected = std::fs::read_to_string(verdicts).unwrap();
    let expected: Vec<&str> = expected.lines().collect();
    assert_eq!(expected.len(), checkpoint.transactions.len());
    for (i, tx) in checkpoint.transactions.iter().enumerate() {
        let line = expected[i];
        let bump = Bump::with_capacity(1 << 16);
        let signed = &tx.transaction;
        let validity = match validation::sender_signed::validity_check(signed, &ctx, &bump) {
            Ok(checked) => format!("ok:{}", checked.tx_size),
            Err(e) => format!("{:?}", e.kind),
        };
        let verification = validation::sender_signed::deserialization_checks(signed, &bump)
            .and_then(|(signatures, _)| {
                validation::verify::verify_signatures(
                    signed,
                    signatures,
                    epoch,
                    &verifier,
                    &[],
                    &bump,
                )
            })
            .map_or_else(|e| format!("{:?}", e.kind), |()| "ok".to_owned());
        let ours = format!("{i} {validity} {verification}");
        if ours != line {
            mismatches.push(format!(
                "{} tx {i}: reference {line}, ours {ours}",
                chk.display()
            ));
        }
    }
    checkpoint.transactions.len()
}

#[test]
fn checked_in_checkpoint() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut mismatches = vec![];
    let n = check(
        &data.join("../messages/tests/data/mainnet-325300367.chk"),
        &data.join("tests/data/mainnet-325300367.validity"),
        &mut mismatches,
    );
    assert!(n > 0);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn fetched_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/mainnet");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut chks: Vec<PathBuf> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "chk"))
        .filter(|p| p.with_extension("validity").exists())
        .collect();
    chks.sort();
    let mut mismatches = vec![];
    let mut total = 0;
    for chk in &chks {
        total += check(chk, &chk.with_extension("validity"), &mut mismatches);
    }
    eprintln!("{total} transactions in {} checkpoints", chks.len());
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
