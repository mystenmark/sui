// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signature parsing against the reference's `GenericSignature::from_bytes`,
//! from `sui-oracle --signature-vectors`.

use containers::Bump;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn matches_the_reference() {
    let mut mismatches = vec![];
    let mut count = 0;
    for line in include_str!("data/signatures.vectors").lines() {
        let [label, hex, verdict] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("bad line {line}");
        };
        let bytes = unhex(hex);
        let bump = Bump::with_capacity(4096);
        let ours = match validation::signature::parse(&bytes, &bump) {
            Ok((parsed, len)) => format!("{}:{len}", parsed.variant()),
            Err(_) => "error".to_owned(),
        };
        if ours != verdict {
            mismatches.push(format!("{label}: reference {verdict}, ours {ours}"));
        }
        count += 1;
    }
    assert!(count > 0);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
