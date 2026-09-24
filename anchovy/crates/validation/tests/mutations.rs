// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Randomly mutated transactions against the reference's verdicts on
//! everything static a validator checks on submission, from
//! `sui-oracle --mutation-vectors`.

use std::io::Read as _;

mod common;

#[test]
fn matches_the_reference() {
    let mut vectors = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("data/mutations.vectors.gz")[..])
        .read_to_string(&mut vectors)
        .unwrap();
    common::replay(&vectors);
}
