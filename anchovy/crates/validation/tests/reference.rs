// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Our checks against the reference's verdicts, from
//! `sui-oracle --validity-vectors` (format documented in the oracle's
//! `validity.rs`).

use std::collections::HashMap;

use messages::Message;
use messages::base::Digest;
use messages::transaction::TransactionData;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::Context;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn chain(name: &str) -> Chain {
    match name {
        "Mainnet" => Chain::Mainnet,
        "Testnet" => Chain::Testnet,
        "Unknown" => Chain::Unknown,
        _ => panic!("unknown chain {name}"),
    }
}

struct Tx {
    check: String,
    label: String,
    bytes: Vec<u8>,
}

fn run(tx: &Tx, ctx: &Context<'_>) -> String {
    let result = match tx.check.as_str() {
        "tx_data" => {
            let message = Message::<TransactionData>::parse(tx.bytes.clone())
                .map_err(|(e, _)| e)
                .unwrap();
            validation::transaction_data::validity_check(message.get(), ctx)
        }
        check => panic!("unknown check {check}"),
    };
    match result {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("{:?}", e.kind),
    }
}

#[test]
fn matches_the_reference() {
    let mut configs: HashMap<(String, u64), ProtocolConfig> = HashMap::new();
    let mut txs: HashMap<String, Tx> = HashMap::new();
    let mut cases = 0;
    let mut mismatches = Vec::new();

    for line in include_str!("data/validity.vectors").lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields.as_slice() {
            ["tx", id, check, label, hex] => {
                let tx = Tx {
                    check: (*check).to_owned(),
                    label: (*label).to_owned(),
                    bytes: unhex(hex),
                };
                txs.insert((*id).to_owned(), tx);
            }
            [
                "case",
                id,
                chain_name,
                versions,
                epoch,
                chain_id,
                rgp,
                committee,
                verdict,
            ] => {
                let tx = &txs[*id];
                let (first, last) = versions.split_once('-').unwrap();
                let (first, last): (u64, u64) = (first.parse().unwrap(), last.parse().unwrap());
                for version in first..=last {
                    let config = configs
                        .entry(((*chain_name).to_owned(), version))
                        .or_insert_with(|| {
                            ProtocolConfig::get_for_version(
                                ProtocolVersion::new(version),
                                chain(chain_name),
                            )
                        });
                    let ctx = Context {
                        config,
                        epoch: epoch.parse().unwrap(),
                        chain_identifier: Digest::new(unhex(chain_id).try_into().unwrap()),
                        reference_gas_price: rgp.parse().unwrap(),
                        committee_size: committee.parse().unwrap(),
                    };
                    let ours = run(tx, &ctx);
                    if ours != *verdict {
                        mismatches.push(format!(
                            "{} {} v{version} {chain_name} rgp {rgp} committee {committee}: \
                             reference {verdict}, ours {ours}",
                            tx.check, tx.label
                        ));
                    }
                    cases += 1;
                }
            }
            _ => panic!("bad vector line: {line}"),
        }
    }
    assert!(cases > 0);
    assert!(
        mismatches.is_empty(),
        "{} of {cases} cases differ:\n{}",
        mismatches.len(),
        mismatches[..mismatches.len().min(30)].join("\n")
    );
}
