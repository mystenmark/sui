// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Parses real mainnet checkpoints. One small checkpoint is checked in;
//! `scripts/fetch-mainnet.sh` fills `corpus/mainnet/` with more.

use std::path::{Path, PathBuf};

use anchovy_types::Message;
use anchovy_types::checkpoint::{CheckpointData, VersionedCheckpointContents};
use anchovy_types::effects::{
    ChangeKind, ObjectOut, TransactionEffects, TransactionEvents, VersionedEffects,
};
use anchovy_types::object::{Data, Object};
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
    let checkpoint = Message::<CheckpointData>::parse(read_chk(path))
        .unwrap_or_else(|(e, _)| panic!("{}: {e}", path.display()));
    counts.wire += checkpoint.wire_bytes().len();
    counts.arena += checkpoint.arena_size();

    // Digests computed while parsing must be the ones the checkpoint records.
    let view = checkpoint.get();
    assert_eq!(
        view.checkpoint_contents.digest(),
        *view.checkpoint_summary.data.content_digest
    );
    let VersionedCheckpointContents::V2(contents) = &view.checkpoint_contents.version else {
        panic!("{}: mainnet checkpoints have V2 contents", path.display())
    };
    assert_eq!(contents.len(), view.transactions.len());

    for (i, tx) in view.transactions.iter().enumerate() {
        counts.transactions += 1;
        assert_eq!(contents[i].digest.transaction, *tx.transaction.digest());
        assert_eq!(contents[i].digest.effects, tx.effects.digest);
        let (transaction_digest, events_digest) = match &tx.effects.version {
            VersionedEffects::V2(v2) => (v2.transaction_digest, v2.events_digest),
            VersionedEffects::V1(v1) => (v1.transaction_digest, v1.events_digest),
        };
        assert_eq!(*transaction_digest, tx.transaction.data.digest);
        assert_eq!(events_digest.copied(), tx.events.map(|e| e.digest()));
        for object in tx.output_objects {
            let digest = object.digest();
            let written = match &tx.effects.version {
                VersionedEffects::V2(v2) => v2.changed_objects.iter().any(|c| {
                    matches!(c.output_state, ObjectOut::ObjectWrite(d, _) if *d == digest)
                        || matches!(c.output_state, ObjectOut::PackageWrite(_, d) if *d == digest)
                }),
                VersionedEffects::V1(_) => true,
            };
            assert!(
                written,
                "{}: output object digest not in effects",
                path.display()
            );
        }

        let data = Message::<TransactionData>::parse(tx.transaction.data.bytes.to_vec()).unwrap();
        assert_eq!(*data.get(), tx.transaction.data);

        let effects = Message::<TransactionEffects>::parse(tx.effects.bytes.to_vec()).unwrap();
        assert_eq!(*effects.get(), tx.effects);

        // Executed effects only hold changes the reference has a class for,
        // and gas is paid from an object that existed.
        if let VersionedEffects::V2(v2) = &tx.effects.version {
            for change in v2.changed_objects {
                assert_ne!(change.kind, ChangeKind::Unclassified, "{}", path.display());
            }
            if let Some(i) = v2.gas_object_index {
                let kind = v2.changed_objects[i as usize].kind;
                assert!(matches!(kind, ChangeKind::Mutated | ChangeKind::Deleted));
            }
            let created = v2.changes(ChangeKind::Created).count();
            let output_created = tx
                .output_objects
                .iter()
                .filter(|o| {
                    let id = match &o.data {
                        Data::Move(m) => &m.contents[..32],
                        Data::Package(p) => &p.id.0[..],
                    };
                    v2.changes(ChangeKind::Created).any(|c| c.id.0 == id)
                })
                .count();
            assert_eq!(created, output_created, "{}", path.display());
        }

        if let Some(events) = tx.events {
            let alone = Message::<TransactionEvents>::parse(events.bytes.to_vec()).unwrap();
            assert_eq!(*alone.get(), events);
        }

        for object in tx.input_objects.iter().chain(tx.output_objects) {
            counts.objects += 1;
            let alone = Message::<Object>::parse(object.bytes.to_vec()).unwrap();
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
