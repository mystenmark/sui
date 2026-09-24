// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Our checks against the reference's verdicts, from
//! `sui-oracle --validity-vectors`.

mod common;

#[test]
fn matches_the_reference() {
    common::replay(include_str!("data/validity.vectors"));
}
