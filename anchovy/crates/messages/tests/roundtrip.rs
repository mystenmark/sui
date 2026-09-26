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

/// A submitted `Transaction` is its `SenderSignedData`'s bytes, one
/// container deeper.
#[test]
fn transaction_envelope() {
    use messages::transaction::{DigestReady, SenderSignedData, Transaction};
    let mut bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/mainnet-325300367.chk"
    ))
    .unwrap();
    bytes.remove(0);
    let checkpoint = Message::<messages::checkpoint::CheckpointData>::parse(bytes).unwrap();
    for tx in checkpoint.get().transactions {
        let wire = tx.transaction.bytes.to_vec();
        let envelope = Message::<Transaction<DigestReady>>::parse(wire.clone()).unwrap();
        let bare = Message::<SenderSignedData>::parse(wire).unwrap();
        assert_eq!(envelope.get().0, *bare.get());
    }
}

/// A transaction parsed without its digest, then hashed, is the one parsed
/// with it, singly or a batch at a time; a batch keeps its allocation.
#[test]
fn transaction_digest_computed_later() {
    use messages::transaction::{DigestPending, DigestReady, Transaction};
    let mut bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/mainnet-325300367.chk"
    ))
    .unwrap();
    bytes.remove(0);
    let checkpoint = Message::<messages::checkpoint::CheckpointData>::parse(bytes).unwrap();
    let transactions = checkpoint.get().transactions;
    let pending: Vec<Message<Transaction<DigestPending>>> = transactions
        .iter()
        .map(|tx| Message::parse(tx.transaction.bytes.to_vec()).unwrap())
        .collect();
    let singly: Vec<Message<Transaction<DigestReady>>> = transactions
        .iter()
        .map(|tx| {
            let pending: Message<Transaction<DigestPending>> =
                Message::parse(tx.transaction.bytes.to_vec()).unwrap();
            pending.with_digest()
        })
        .collect();
    let (ptr, capacity) = (pending.as_ptr().cast::<u8>(), pending.capacity());
    let ready = Message::with_digests(pending);
    assert_eq!(
        (ready.as_ptr().cast::<u8>(), ready.capacity()),
        (ptr, capacity)
    );
    assert_eq!(ready.len(), transactions.len());
    for (i, ready) in ready.iter().enumerate() {
        let tx = &transactions[i].transaction;
        assert_eq!(ready.get().0, *tx);
        assert_eq!(ready.get().0.digest(), tx.digest());
        assert_eq!(singly[i].get(), ready.get());
    }
}

/// The envelope's extra container level, at the depth limit, against the
/// reference (`sui-oracle --depth-vectors`).
#[test]
fn transaction_envelope_depth() {
    use messages::transaction::{DigestPending, SenderSignedData, Transaction};
    for line in include_str!("data/depth.txt").lines() {
        let [label, hex, bare, envelope] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("bad line {line}");
        };
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let verdict = |ok: bool| if ok { "ok" } else { "error" };
        let ours_bare = verdict(Message::<SenderSignedData>::parse(bytes.clone()).is_ok());
        let ours_envelope = verdict(Message::<Transaction<DigestPending>>::parse(bytes).is_ok());
        assert_eq!((ours_bare, ours_envelope), (bare, envelope), "{label}");
    }
}
