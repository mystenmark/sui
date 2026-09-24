// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Replays validity vectors (format in the oracle's `validity.rs`) against
//! our checks.

use std::collections::HashMap;

use fastcrypto_zkp::bn254::zk_login::{JWK, JwkId};
use messages::Message;
use messages::base::Digest;
use messages::transaction::{SenderSignedData, TransactionData};
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::Context;
use validation::verify::Verifier;

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

/// A vector's transaction, parsed once as its check needs it.
enum Parsed {
    Data(Message<TransactionData<'static>>),
    Signed(Message<SenderSignedData<'static>>),
    /// Our parser rejects it.
    Undecodable,
}

pub struct Tx {
    check: String,
    label: String,
    parsed: Parsed,
}

impl Tx {
    fn new(check: &str, label: &str, bytes: Vec<u8>) -> Tx {
        let parsed = match check {
            "tx_data" | "gas_price" => Parsed::Data(
                Message::<TransactionData>::parse(bytes)
                    .map_err(|(e, _)| e)
                    .unwrap(),
            ),
            _ => Message::<SenderSignedData>::parse(bytes)
                .map_or(Parsed::Undecodable, Parsed::Signed),
        };
        Tx {
            check: check.to_owned(),
            label: label.to_owned(),
            parsed,
        }
    }

    fn data(&self) -> &TransactionData<'_> {
        match &self.parsed {
            Parsed::Data(m) => m.get(),
            _ => panic!("not transaction data"),
        }
    }

    fn signed(&self) -> Option<&SenderSignedData<'_>> {
        match &self.parsed {
            Parsed::Signed(m) => Some(m.get()),
            _ => None,
        }
    }
}

fn run(tx: &Tx, ctx: &Context<'_>, verifier: &Verifier) -> String {
    let result = match tx.check.as_str() {
        "tx_data" => {
            let bump = containers::Bump::with_capacity(4096);
            validation::transaction_data::validity_check(tx.data(), ctx, &bump)
        }
        "gas_price" => validation::transaction_data::check_gas_price(tx.data().gas_data.price, ctx),
        "sender_signed" => {
            let Some(signed) = tx.signed() else {
                return "TransactionDeserializationError".to_owned();
            };
            let bump = containers::Bump::with_capacity(4096);
            return match validation::sender_signed::validity_check(signed, ctx, &bump) {
                Ok(checked) => format!("ok:{}", checked.tx_size),
                Err(e) => format!("{:?}", e.kind),
            };
        }
        "verify" => {
            let Some(signed) = tx.signed() else {
                return "TransactionDeserializationError".to_owned();
            };
            let bump = containers::Bump::with_capacity(4096);
            let (signatures, _) = validation::sender_signed::deserialization_checks(signed, &bump)
                .expect("verification vectors decode");
            validation::verify::verify_signatures(
                signed,
                signatures,
                ctx.epoch,
                verifier,
                &[],
                &bump,
            )
        }
        "full" => {
            let Some(signed) = tx.signed() else {
                return "TransactionDeserializationError".to_owned();
            };
            let bump = containers::Bump::with_capacity(1 << 16);
            return match validation::check(signed, ctx, verifier, &[], &bump) {
                Ok(checked) => format!("ok:{}", checked.tx_size),
                Err(e) => format!("{:?}", e.kind),
            };
        }
        check => panic!("unknown check {check}"),
    };
    match result {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("{:?}", e.kind),
    }
}

/// Replays `vectors`, panicking with the differences if any.
pub fn replay(vectors: &str) {
    let mut configs: HashMap<(String, u64), ProtocolConfig> = HashMap::new();
    let mut verifiers: HashMap<(String, u64), Verifier> = HashMap::new();
    let mut jwks: Vec<(JwkId, JWK)> = vec![];
    let mut txs: HashMap<String, Tx> = HashMap::new();
    let mut cases = 0;
    let mut mismatches = Vec::new();

    let mut reference_panics = vec![];
    for line in vectors.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if let Some(json) = line.strip_prefix("jwks ") {
            jwks = serde_json::from_str(json).unwrap();
            continue;
        }
        match fields.as_slice() {
            ["tx", id, check, label, hex] => {
                let tx = Tx::new(check, label, unhex(hex));
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
                    let verifier = verifiers
                        .entry(((*chain_name).to_owned(), version))
                        .or_insert_with(|| Verifier::new(config, chain(chain_name), jwks.clone()));
                    let ours = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(tx, &ctx, verifier)
                    }))
                    .unwrap_or_else(|_| "our panic".to_owned());
                    if *verdict == "panic" && tx.check == "full" {
                        // The reference itself crashed: a finding, not a verdict.
                        reference_panics.push(format!("{} v{version} {chain_name}", tx.label));
                        continue;
                    }
                    // The reference's checks passed; what follows them panicked.
                    let expected = if *verdict == "panic" { "ok" } else { verdict };
                    if ours != expected {
                        mismatches.push(format!(
                            "{} {}: reference {verdict}, ours {ours} \
                             (v{version} {chain_name} rgp {rgp} committee {committee})",
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
    if !reference_panics.is_empty() {
        eprintln!(
            "the reference panicked on {} cases, e.g. {}",
            reference_panics.len(),
            reference_panics[0]
        );
    }
    // One line per transaction and verdict pair, with its first case.
    let mut first_of_each: Vec<&String> = vec![];
    let mut seen = std::collections::HashSet::new();
    for m in &mismatches {
        if seen.insert(m.split(" (").next().unwrap()) {
            first_of_each.push(m);
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {cases} cases differ, {} distinct:\n{}",
        mismatches.len(),
        first_of_each.len(),
        first_of_each
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
