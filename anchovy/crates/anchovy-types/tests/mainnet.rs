// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Parses real mainnet checkpoints. One small checkpoint is checked in;
//! `scripts/fetch-mainnet.sh` fills `corpus/mainnet/` with more.

use std::path::{Path, PathBuf};

use anchovy_types::Message;
use anchovy_types::checkpoint::CheckpointData;
use anchovy_types::effects::{TransactionEffects, TransactionEvents};
use anchovy_types::object::Object;
use anchovy_types::transaction::TransactionData;

/// The bytes of a `.chk` file after its one-byte encoding tag.
fn read_chk(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes[0], 1, "{}: not a BCS blob", path.display());
    bytes.remove(0);
    bytes
}

struct Counts {
    transactions: usize,
    objects: usize,
    wire: usize,
    arena: usize,
}

/// Parses a checkpoint, then parses each hashed part on its own from the
/// span the checkpoint recorded for it and expects an equal view.
fn check(path: &Path, counts: &mut Counts) {
    let checkpoint = Message::<CheckpointData<'static>>::parse(read_chk(path))
        .unwrap_or_else(|(e, _)| panic!("{}: {e}", path.display()));
    counts.wire += checkpoint.wire_bytes().len();
    counts.arena += checkpoint.arena_size();

    for tx in checkpoint.get().transactions {
        counts.transactions += 1;

        let data =
            Message::<TransactionData<'static>>::parse(tx.transaction.data.bytes.to_vec()).unwrap();
        assert_eq!(*data.get(), tx.transaction.data);

        let effects =
            Message::<TransactionEffects<'static>>::parse(tx.effects.bytes.to_vec()).unwrap();
        assert_eq!(*effects.get(), tx.effects);

        if let Some(events) = tx.events {
            let alone =
                Message::<TransactionEvents<'static>>::parse(events.bytes.to_vec()).unwrap();
            assert_eq!(*alone.get(), events);
        }

        for object in tx.input_objects.iter().chain(tx.output_objects) {
            counts.objects += 1;
            let alone = Message::<Object<'static>>::parse(object.bytes.to_vec()).unwrap();
            assert_eq!(*alone.get(), *object);
        }
    }
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

#[test]
fn checked_in_checkpoint() {
    let mut counts = Counts {
        transactions: 0,
        objects: 0,
        wire: 0,
        arena: 0,
    };
    check(&data_dir().join("mainnet-325300367.chk"), &mut counts);
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
    let mut counts = Counts {
        transactions: 0,
        objects: 0,
        wire: 0,
        arena: 0,
    };
    let mut checkpoints = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "chk") {
            check(&path, &mut counts);
            checkpoints += 1;
        }
    }
    eprintln!(
        "{checkpoints} checkpoints, {} transactions, {} objects, {} wire bytes, {} arena bytes",
        counts.transactions, counts.objects, counts.wire, counts.arena
    );
}
