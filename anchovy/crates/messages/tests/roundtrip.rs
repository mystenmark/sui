// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Mainnet checkpoints through the `build` types: `bcs` must decode them and
//! encode the same bytes back, and every view must convert to what `bcs`
//! decoded. `scripts/fetch-mainnet.sh` fills `corpus/mainnet/`.

use std::fmt::Debug;
use std::path::Path;

use messages::checkpoint::CheckpointData;
use messages::effects::{TransactionEffects, TransactionEvents};
use messages::object::Object;
use messages::signature::MultiSig;
use messages::transaction::TransactionData;
use messages::{Message, Wire, build};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The bytes of a `.chk` file after its one-byte encoding tag.
fn read_chk(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes[0], 1, "{}: not a BCS blob", path.display());
    bytes.remove(0);
    bytes
}

/// Checks one encoding of `T` against its mirror `B`.
fn check<T, B>(bytes: &[u8])
where
    T: Wire,
    B: Serialize + DeserializeOwned + PartialEq + Debug + for<'a, 'v> From<&'a T::View<'v>>,
{
    let decoded: B = bcs::from_bytes(bytes).unwrap();
    assert_eq!(bcs::to_bytes(&decoded).unwrap(), bytes);

    let message = Message::<T>::parse(bytes.to_vec()).unwrap_or_else(|(e, _)| panic!("{e}"));
    assert_eq!(B::from(message.get()), decoded);
}

#[derive(Default)]
struct Counts {
    checkpoints: usize,
    transactions: usize,
    events: usize,
    objects: usize,
}

fn check_checkpoint(path: &Path, counts: &mut Counts) {
    let bytes = read_chk(path);
    check::<CheckpointData<'static>, build::checkpoint::CheckpointData>(&bytes);
    counts.checkpoints += 1;

    let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
    for tx in checkpoint.get().transactions {
        counts.transactions += 1;
        check::<TransactionData<'static>, build::transaction::TransactionData>(
            tx.transaction.data.bytes,
        );
        check::<TransactionEffects<'static>, build::effects::TransactionEffects>(tx.effects.bytes);
        if let Some(events) = tx.events {
            counts.events += 1;
            check::<TransactionEvents<'static>, build::effects::TransactionEvents>(events.bytes);
        }
        for object in tx.input_objects.iter().chain(tx.output_objects) {
            counts.objects += 1;
            check::<Object<'static>, build::object::Object>(object.bytes);
        }
    }
}

#[test]
fn checked_in_checkpoint() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mainnet-325300367.chk");
    let mut counts = Counts::default();
    check_checkpoint(&path, &mut counts);
    assert!(counts.transactions > 0 && counts.objects > 0);
}

#[test]
fn corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/mainnet");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!(
            "no corpus at {}; run scripts/fetch-mainnet.sh",
            dir.display()
        );
        return;
    };
    let mut counts = Counts::default();
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "chk") {
            check_checkpoint(&path, &mut counts);
        }
    }
    eprintln!(
        "{} checkpoints, {} transactions, {} with events, {} objects",
        counts.checkpoints, counts.transactions, counts.events, counts.objects
    );
}

/// The corpus has no multisig, so one is built here.
#[test]
fn multisig() {
    use build::signature::{
        CompressedSignature, MultiSigPublicKey, PublicKey, ZkLoginAuthenticatorAsBytes,
        ZkLoginPublicIdentifier,
    };

    let built = build::signature::MultiSig {
        sigs: vec![
            CompressedSignature::Ed25519([1; 64]),
            CompressedSignature::Secp256k1([2; 64]),
            CompressedSignature::Secp256r1([3; 64]),
            CompressedSignature::ZkLogin(ZkLoginAuthenticatorAsBytes(vec![4; 200])),
        ],
        bitmap: 0b1111,
        multisig_pk: MultiSigPublicKey {
            pk_map: vec![
                (PublicKey::Ed25519([5; 32]), 1),
                (PublicKey::Secp256k1([6; 33]), 2),
                (PublicKey::Secp256r1([7; 33]), 3),
                (PublicKey::ZkLogin(ZkLoginPublicIdentifier(vec![8; 40])), 4),
            ],
            threshold: 5,
        },
    };
    let bytes = bcs::to_bytes(&built).unwrap();
    assert_eq!(
        bytes.len(),
        1 + 3 * 65 + (1 + 2 + 200) + 2 + 1 + 34 + 35 + 35 + 43 + 2
    );
    assert_eq!(
        bcs::from_bytes::<build::signature::MultiSig>(&bytes).unwrap(),
        built
    );

    let message = Message::<MultiSig>::parse(bytes).unwrap();
    assert_eq!(
        build::signature::MultiSig::try_from(message.get()),
        Ok(built)
    );
}

/// The known gap: the views take a passkey, the snapshot-shaped mirrors do not.
#[test]
fn multisig_with_passkey() {
    // One `Passkey` signature of no bytes, no bitmap bits, no keys.
    let bytes = vec![1, 4, 0, 0, 0, 0, 0, 0];
    assert!(bcs::from_bytes::<build::signature::MultiSig>(&bytes).is_err());

    let message = Message::<MultiSig>::parse(bytes).unwrap();
    assert_eq!(
        build::signature::MultiSig::try_from(message.get()),
        Err(build::signature::PasskeyUnsupported)
    );
}
