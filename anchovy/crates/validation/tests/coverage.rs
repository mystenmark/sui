// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Every error our checks can return is the reference's verdict in some
//! vector, so each rejection path is compared at least once. A kind that
//! cannot be reached is listed with the reason.

use std::collections::HashSet;
use std::io::Read as _;

use validation::ErrorKind;

/// Kinds no transaction can produce end to end.
const UNREACHABLE: &[(ErrorKind, &str)] = &[
    (
        ErrorKind::IncorrectSigner,
        "a signature's signer is its own key's address, which is what the check compares",
    ),
    (
        ErrorKind::InvalidAddress,
        "a zkLogin signature's signer is its own address; nested in a multisig the \
         reference reports InvalidSignature",
    ),
];

#[test]
fn every_error_kind_has_a_vector() {
    let mut mutations = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("data/mutations.vectors.gz")[..])
        .read_to_string(&mut mutations)
        .unwrap();
    let files = [
        include_str!("data/validity.vectors"),
        &mutations,
        include_str!("data/mainnet-325300367.validity"),
    ];
    let seen: HashSet<&str> = files
        .iter()
        .flat_map(|f| f.lines())
        .flat_map(|line| line.split(' '))
        .map(|verdict| verdict.split(':').next().unwrap())
        .collect();

    let mut missing = vec![];
    for kind in ErrorKind::ALL {
        let name = format!("{kind:?}");
        let unreachable = UNREACHABLE.iter().any(|(k, _)| k == kind);
        match (seen.contains(name.as_str()), unreachable) {
            (false, false) => missing.push(format!("{name}: no vector")),
            (true, true) => missing.push(format!("{name}: listed unreachable, but has a vector")),
            _ => {}
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
