// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Holds the hand-written parsers to `bcs::from_bytes` on the builder types.
//! For any input the two must agree on whether it parses, and when it does
//! they must hold the same value, which must serialize back to the input.
//! The first byte picks the type.

#![no_main]

use messages::build;
use messages::checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointData, FullCheckpointContents,
};
use messages::effects::{TransactionEffects, TransactionEvents};
use messages::object::Object;
use messages::signature::{CompressedSignature, MultiSig, PublicKey};
use messages::transaction::{SenderSignedData, TransactionData};
use messages::{Message, Wire};
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

/// The builders mirror the format snapshot, which lacks the `Passkey`
/// variants the view accepts; an input the view accepts because of one is
/// not a disagreement.
fn compare_multisig(bytes: &[u8]) {
    let view = Message::<MultiSig>::parse(bytes.to_vec());
    let owned = bcs::from_bytes::<build::signature::MultiSig>(bytes);
    match (view, owned) {
        (Ok(view), Ok(owned)) => {
            assert_eq!(
                build::signature::MultiSig::try_from(view.get()).unwrap(),
                owned
            );
            assert_eq!(bcs::to_bytes(&owned).unwrap(), bytes);
        }
        (Err(_), Err(_)) => {}
        (Ok(view), Err(e)) => {
            let v = view.get();
            let passkey = v
                .sigs
                .iter()
                .any(|s| matches!(s, CompressedSignature::Passkey(_)))
                || v.multisig_pk
                    .pk_map
                    .iter()
                    .any(|(k, _)| matches!(k, PublicKey::Passkey(_)));
            assert!(passkey, "only the view parser accepts: bcs says {e}");
        }
        (Err((e, _)), Ok(_)) => panic!("only bcs accepts: the view parser says {e}"),
    }
}

fuzz_target!(|input: &[u8]| {
    let Some((&selector, bytes)) = input.split_first() else {
        return;
    };
    match selector % 10 {
        0 => compare::<SenderSignedData, build::transaction::SenderSignedData>(bytes),
        1 => compare::<TransactionData, build::transaction::TransactionData>(bytes),
        2 => compare::<TransactionEffects, build::effects::TransactionEffects>(bytes),
        3 => compare::<TransactionEvents, build::effects::TransactionEvents>(bytes),
        4 => compare::<Object, build::object::Object>(bytes),
        5 => compare::<CheckpointContents, build::checkpoint::CheckpointContents>(bytes),
        6 => compare::<CertifiedCheckpointSummary, build::checkpoint::CertifiedCheckpointSummary>(
            bytes,
        ),
        7 => compare::<FullCheckpointContents, build::checkpoint::FullCheckpointContents>(bytes),
        8 => compare::<CheckpointData, build::checkpoint::CheckpointData>(bytes),
        _ => compare_multisig(bytes),
    }
});
