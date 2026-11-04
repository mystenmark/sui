// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validation rejects with the reference's error, fields and all: each
//! vector's validity check fails with the bytes the reference's does.

mod common;

use common::Case;
use containers::Bump;
use sui_types::digests::{ChainIdentifier, CheckpointDigest};
use sui_types::transaction::{Transaction, TxValidityCheckContext};
use validator::epoch::EpochState;

/// The cases whose reasons differ from the reference's, as labels with
/// both reasons; and how many rejections were compared.
fn differences(cases: &[Case], epoch: &EpochState) -> (Vec<String>, usize) {
    let context = epoch.context();
    let reference_context = TxValidityCheckContext {
        config: context.config,
        epoch: context.epoch,
        chain_identifier: ChainIdentifier::from(CheckpointDigest::new(
            context.chain_identifier.bytes,
        )),
        reference_gas_price: context.reference_gas_price,
        committee_size: context.committee_size,
    };
    let mut differences = vec![];
    let mut compared = 0;
    for case in cases {
        // A transaction the reference cannot decode fails with its decoder's
        // message, which anchovy's parser does not reproduce.
        let Ok(reference) = bcs::from_bytes::<Transaction>(&case.bytes) else {
            continue;
        };
        let reference = reference.data().validity_check(&reference_context);
        let transaction = case.parse().unwrap();
        let bump = Bump::with_capacity(1 << 16);
        let ours = validation::sender_signed::validity_check(&transaction.get().0, &context, &bump);
        let same = match (&ours, &reference) {
            (Ok(_), Ok(_)) => true,
            (Err(ours), Err(reference)) => {
                compared += 1;
                bcs::to_bytes(&ours.reason).unwrap() == bcs::to_bytes(reference).unwrap()
            }
            _ => false,
        };
        if !same {
            differences.push(format!(
                "{}: ours {:?}, the reference's {:?}",
                case.label,
                ours.map(|_| ()).map_err(|e| e.reason),
                reference.map(|_| ())
            ));
        }
    }
    (differences, compared)
}

#[test]
fn vectors_are_rejected_with_the_reference_s_reasons() {
    let (cases, jwks) = common::signed_vectors();
    let (differences, compared) = differences(&cases, &common::vectors_epoch(4, jwks));
    assert!(compared > 0);
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

#[test]
fn mainnet_transactions_are_judged_as_the_reference_judges_them() {
    let (cases, epoch) = common::mainnet();
    let (differences, compared) = differences(&cases, &common::mainnet_epoch(epoch));
    assert!(compared > 0);
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// The vectors' transactions whose validity checks both reject, in every
/// case's context on `only_chain` at its last protocol version: those whose
/// reasons differ, and how many were compared.
fn vector_differences(vectors: &str, only_chain: &str) -> (Vec<String>, usize) {
    use std::collections::HashMap;
    use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
    use sui_types::transaction::{SenderSignedData, TransactionData, TransactionDataAPI as _};

    let mut configs: HashMap<(String, u64), ProtocolConfig> = HashMap::new();
    let mut txs: HashMap<&str, (&str, &str, Vec<u8>)> = HashMap::new();
    let mut differences = vec![];
    let mut compared = 0;
    for line in vectors.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            ["tx", id, check, label, hex] => {
                txs.insert(id, (check, label, common::unhex(hex)));
            }
            [
                "case",
                id,
                chain,
                versions,
                epoch,
                chain_id,
                rgp,
                committee,
                _,
            ] if chain == only_chain => {
                let (check, label, bytes) = &txs[id];
                let version: u64 = versions.split_once('-').unwrap().1.parse().unwrap();
                let config = configs
                    .entry((chain.to_owned(), version))
                    .or_insert_with(|| {
                        let chain = match chain {
                            "Mainnet" => Chain::Mainnet,
                            "Testnet" => Chain::Testnet,
                            _ => Chain::Unknown,
                        };
                        ProtocolConfig::get_for_version(ProtocolVersion::new(version), chain)
                    });
                let chain_id: [u8; 32] = common::unhex(chain_id).try_into().unwrap();
                let context = validation::Context {
                    config,
                    epoch: epoch.parse().unwrap(),
                    chain_identifier: messages::base::Digest::new(chain_id),
                    reference_gas_price: rgp.parse().unwrap(),
                    committee_size: committee.parse().unwrap(),
                };
                let reference_context = TxValidityCheckContext {
                    config,
                    epoch: context.epoch,
                    chain_identifier: ChainIdentifier::from(CheckpointDigest::new(chain_id)),
                    reference_gas_price: context.reference_gas_price,
                    committee_size: context.committee_size,
                };
                let bump = Bump::with_capacity(1 << 16);
                let (ours, reference) = match *check {
                    "tx_data" => {
                        let Ok(reference) = bcs::from_bytes::<TransactionData>(bytes) else {
                            continue;
                        };
                        let data = messages::Message::<
                            messages::transaction::TransactionData<'static>,
                        >::parse(bytes.clone())
                        .map_err(|(e, _)| e)
                        .unwrap();
                        (
                            validation::transaction_data::validity_check(
                                data.get(),
                                &context,
                                &bump,
                            )
                            .err(),
                            reference.validity_check(&reference_context).err(),
                        )
                    }
                    "sender_signed" | "full" => {
                        let Ok(reference) = bcs::from_bytes::<SenderSignedData>(bytes) else {
                            continue;
                        };
                        let Ok(signed) = messages::Message::<
                            messages::transaction::SenderSignedData<'static>,
                        >::parse(bytes.clone()) else {
                            continue;
                        };
                        (
                            validation::sender_signed::validity_check(
                                signed.get(),
                                &context,
                                &bump,
                            )
                            .err(),
                            reference.validity_check(&reference_context).err(),
                        )
                    }
                    _ => continue,
                };
                let (Some(ours), Some(reference)) = (ours, reference) else {
                    continue;
                };
                compared += 1;
                if bcs::to_bytes(&ours.reason).unwrap() != bcs::to_bytes(&reference).unwrap() {
                    differences.push(format!(
                        "{label} v{version} {chain}: ours {:?}, the reference's {reference:?}",
                        ours.reason
                    ));
                }
            }
            _ => {}
        }
    }
    (differences, compared)
}

/// Points the reference's gasless token cache at a version other than the
/// one `chain`'s first case reads, so that case parses its own chain's.
fn reload_gasless_tokens(vectors: &str, chain: &str) {
    use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
    let first = vectors
        .lines()
        .find_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields[0] == "case" && fields[2] == chain)
                .then(|| fields[3].split_once('-').unwrap().1)
        })
        .map(|v| v.parse::<u64>().unwrap());
    let config = (1..=ProtocolVersion::MAX.as_u64())
        .rev()
        .filter(|v| Some(*v) != first)
        .map(|v| ProtocolConfig::get_for_version(ProtocolVersion::new(v), Chain::Unknown))
        .find(ProtocolConfig::enable_gasless)
        .unwrap();
    sui_types::transaction::get_gasless_allowed_token_types(&config);
}

fn assert_no_vector_differences(vectors: &str) {
    // One chain at a time: the reference caches gasless token types by
    // protocol version alone, as a process serves one chain.
    let (mut differences, mut compared) = (vec![], 0);
    for chain in ["Mainnet", "Testnet", "Unknown"] {
        reload_gasless_tokens(vectors, chain);
        let (d, c) = vector_differences(vectors, chain);
        differences.extend(d);
        compared += c;
    }
    assert!(compared > 0);
    // One line per transaction, with its first case.
    let mut seen = std::collections::HashSet::new();
    let distinct: Vec<&String> = differences
        .iter()
        .filter(|d| seen.insert(d.split(' ').next().unwrap()))
        .collect();
    assert!(
        differences.is_empty(),
        "{} of {compared} differ, {} distinct:\n{}",
        differences.len(),
        distinct.len(),
        distinct
            .iter()
            .map(|d| d.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn validity_vectors_are_rejected_with_the_reference_s_reasons() {
    assert_no_vector_differences(include_str!("../../validation/tests/data/validity.vectors"));
}

#[test]
fn mutated_transactions_are_rejected_with_the_reference_s_reasons() {
    use std::io::Read as _;
    let mut vectors = String::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../validation/tests/data/mutations.vectors.gz")[..],
    )
    .read_to_string(&mut vectors)
    .unwrap();
    assert_no_vector_differences(&vectors);
}
