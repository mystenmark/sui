// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Holds the hand-written parsers to `bcs::from_bytes` on the builder types.
//! For any input the two must agree on whether it parses, and when it does
//! they must hold the same value, which must serialize back to the input.
//! The first byte picks the type.

#![no_main]

use anchovy_types::build;
use anchovy_types::checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointData, FullCheckpointContents,
};
use anchovy_types::effects::{TransactionEffects, TransactionEvents};
use anchovy_types::object::Object;
use anchovy_types::transaction::{SenderSignedData, TransactionData};
use anchovy_types::{Message, Wire};
use libfuzzer_sys::fuzz_target;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn compare<T, B>(bytes: &[u8])
where
    T: Wire,
    B: DeserializeOwned + Serialize + PartialEq + std::fmt::Debug,
    for<'v, 'a> B: From<&'v T::View<'a>>,
{
    let view = Message::<T>::parse(bytes.to_vec());
    let owned = bcs::from_bytes::<B>(bytes);
    match (view, owned) {
        (Ok(view), Ok(owned)) => {
            assert_eq!(B::from(view.get()), owned);
            assert_eq!(bcs::to_bytes(&owned).unwrap(), bytes);
        }
        (Err(_), Err(_)) => {}
        (Ok(_), Err(e)) => panic!("only the view parser accepts: bcs says {e}"),
        (Err((e, _)), Ok(_)) => panic!("only bcs accepts: the view parser says {e}"),
    }
}

fuzz_target!(|input: &[u8]| {
    let Some((&selector, bytes)) = input.split_first() else {
        return;
    };
    match selector % 9 {
        0 => compare::<SenderSignedData<'static>, build::transaction::SenderSignedData>(bytes),
        1 => compare::<TransactionData<'static>, build::transaction::TransactionData>(bytes),
        2 => compare::<TransactionEffects<'static>, build::effects::TransactionEffects>(bytes),
        3 => compare::<TransactionEvents<'static>, build::effects::TransactionEvents>(bytes),
        4 => compare::<Object<'static>, build::object::Object>(bytes),
        5 => compare::<CheckpointContents<'static>, build::checkpoint::CheckpointContents>(bytes),
        6 => compare::<
            CertifiedCheckpointSummary<'static>,
            build::checkpoint::CertifiedCheckpointSummary,
        >(bytes),
        7 => compare::<FullCheckpointContents<'static>, build::checkpoint::FullCheckpointContents>(
            bytes,
        ),
        _ => compare::<CheckpointData<'static>, build::checkpoint::CheckpointData>(bytes),
    }
});
