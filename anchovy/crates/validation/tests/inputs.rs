// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The stateful input checks against the reference's verdicts, from
//! `sui-oracle --input-vectors` (format in the oracle's `inputs.rs`).

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::object::Object;
use messages::transaction::SenderSignedData;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::Context;
use validation::inputs::Objects;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[derive(Default)]
struct Store {
    versions: HashMap<ObjectId, BTreeMap<u64, Vec<u8>>>,
    live: HashMap<ObjectId, u64>,
}

fn parse(bytes: &[u8]) -> Message<Object<'static>> {
    Message::<Object>::parse(bytes.to_vec())
        .map_err(|(e, _)| e)
        .unwrap()
}

impl Objects for Store {
    fn live(&self, id: &ObjectId) -> Option<Message<Object<'static>>> {
        self.at(id, *self.live.get(id)?)
    }

    fn at(&self, id: &ObjectId, version: u64) -> Option<Message<Object<'static>>> {
        Some(parse(self.versions.get(id)?.get(&version)?))
    }
}

/// `TransactionData` as unsigned `SenderSignedData`: the input checks never
/// look at signatures.
fn unsigned(data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![1, 0, 0, 0];
    bytes.extend_from_slice(data);
    bytes.push(0);
    bytes
}

#[test]
fn matches_the_reference() {
    let mut vectors = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("data/inputs.vectors.gz")[..])
        .read_to_string(&mut vectors)
        .unwrap();
    let mut config = None;
    let (mut rgp, mut epoch) = (0, 0);
    let mut store = Store::default();
    let mut cases = 0;
    let mut mismatches = vec![];
    for line in vectors.lines() {
        match line.split(' ').collect::<Vec<_>>().as_slice() {
            ["context", version, gas_price, e] => {
                config = Some(ProtocolConfig::get_for_version(
                    ProtocolVersion::new(version.parse().unwrap()),
                    Chain::Unknown,
                ));
                rgp = gas_price.parse().unwrap();
                epoch = e.parse().unwrap();
            }
            ["object", state, hex] => {
                let bytes = unhex(hex);
                let object = parse(&bytes);
                let id = *object.get().id().unwrap();
                let version = object.get().version();
                store.versions.entry(id).or_default().insert(version, bytes);
                if *state == "live" {
                    store.live.insert(id, version);
                }
            }
            ["case", label, hex, verdict] => {
                let ctx = Context {
                    config: config.as_ref().unwrap(),
                    epoch,
                    chain_identifier: Digest::new([0; 32]),
                    reference_gas_price: rgp,
                    committee_size: 1,
                };
                let signed = Message::<SenderSignedData>::parse(unsigned(&unhex(hex)))
                    .map_err(|(e, _)| e)
                    .unwrap();
                let ours = match validation::inputs::check(signed.get(), &ctx, &store) {
                    Ok(()) => "ok".to_owned(),
                    Err(e) => format!("{:?}", e.kind),
                };
                if ours != *verdict {
                    mismatches.push(format!("{label}: reference {verdict}, ours {ours}"));
                }
                cases += 1;
            }
            _ => panic!("bad vector line: {line}"),
        }
    }
    assert!(cases > 0);
    assert!(
        mismatches.is_empty(),
        "{} of {cases} cases differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
