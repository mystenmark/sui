// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Parsing a `ConsensusTransaction` and its decode checks together accept
//! exactly what sui-types' decoder accepts, over the vectors
//! `sui-oracle --consensus-vectors` writes.

use std::io::Read as _;

use containers::Bump;
use messages::Message;
use messages::consensus::ConsensusTransaction;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn decodes_what_sui_decodes() {
    let mut text = String::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../messages/tests/data/consensus.vectors.gz")[..],
    )
    .read_to_string(&mut text)
    .unwrap();
    let mut mismatches = vec![];
    let mut semantic = 0;
    for line in text.lines() {
        let [label, verdict, hex] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("bad line {line}");
        };
        let bump = Bump::with_capacity(1 << 16);
        let ours = Message::<ConsensusTransaction<'static>>::parse(unhex(hex))
            .ok()
            .is_some_and(|m| validation::consensus::decode_checks(m.get(), &bump).is_ok());
        if ours != (verdict == "ok") {
            mismatches.push(format!("{label}: sui {verdict}, ours {ours}"));
        }
        semantic += usize::from(label.starts_with("semantic_"));
    }
    assert!(semantic > 0);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
