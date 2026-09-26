// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Container-depth boundary vectors: a transaction nesting a type argument
//! just deep enough that the reference decodes it as `SenderSignedData`
//! but not as `Transaction` (whose envelope adds one level), and one level
//! shallower, which both decode. One per line:
//!
//! ```text
//! <label> <hex> <SenderSignedData verdict> <Transaction verdict>
//! ```

use std::fmt::Write as _;

use sui_types::transaction::{
    Argument, Command, ProgrammableTransaction, SenderSignedData, Transaction, TransactionKind,
};
use sui_types::type_input::TypeInput;

use crate::validity::Spec;
use crate::validity_signed::signed;

fn with_depth(depth: usize) -> Vec<u8> {
    let ty = (0..depth).fold(TypeInput::U8, |t, _| TypeInput::Vector(Box::new(t)));
    let kind = TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
        inputs: vec![],
        commands: vec![Command::MakeMoveVec(Some(ty), vec![Argument::GasCoin])],
    });
    signed(
        [0, 0, 0],
        &Spec {
            kind,
            ..Spec::new()
        }
        .build(),
        &[&[0; 97]],
    )
}

fn verdict<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> &'static str {
    if bcs::from_bytes::<T>(bytes).is_ok() {
        "ok"
    } else {
        "error"
    }
}

pub fn vectors() -> String {
    // The deepest nesting `SenderSignedData` decodes.
    let deepest = (1..600)
        .take_while(|d| verdict::<SenderSignedData>(&with_depth(*d)) == "ok")
        .last()
        .unwrap();
    let mut out = String::new();
    for (label, depth) in [
        ("below", deepest - 1),
        ("at", deepest),
        ("over", deepest + 1),
    ] {
        let bytes = with_depth(depth);
        writeln!(
            out,
            "{label} {} {} {}",
            crate::hex(&bytes),
            verdict::<SenderSignedData>(&bytes),
            verdict::<Transaction>(&bytes)
        )
        .unwrap();
    }
    out
}
