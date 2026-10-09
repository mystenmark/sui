// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The store round-trips the reference's BCS, tracks live versions, commits
//! atomically, marks genesis and survives reopening. Objects and transactions come from a
//! mainnet checkpoint.

use messages::Message;
use messages::base::Digest;
use messages::checkpoint::CheckpointData;
use store::{Commit, Executed, Live, Store, Written};

fn checkpoint() -> Message<CheckpointData<'static>> {
    let mut bytes = include_bytes!("../../messages/tests/data/mainnet-325300367.chk").to_vec();
    bytes.remove(0);
    Message::parse(bytes).map_err(|(e, _)| e).unwrap()
}

/// The checkpoint's output objects, as writes.
fn written(checkpoint: &CheckpointData<'_>) -> Vec<Written> {
    checkpoint
        .transactions
        .iter()
        .flat_map(|tx| tx.output_objects.iter())
        .map(|object| Written {
            id: *object.id().unwrap(),
            version: object.version(),
            digest: object.digest(),
            bytes: object.bytes.to_vec(),
        })
        .collect()
}

#[test]
fn objects_round_trip_and_follow_their_live_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(!store.has_genesis().unwrap());
    let checkpoint = checkpoint();
    let written = written(checkpoint.get());
    assert!(written.len() > 10);
    let expected: Vec<(Vec<u8>, u64, Digest)> = written
        .iter()
        .map(|w| (w.bytes.clone(), w.version, w.digest))
        .collect();
    let ids: Vec<_> = written.iter().map(|w| w.id).collect();
    store.commit_genesis(written).unwrap();
    assert!(store.has_genesis().unwrap());
    for (i, id) in ids.iter().enumerate() {
        let (bytes, version, digest) = &expected[i];
        let object = store.object(id, *version).unwrap().unwrap();
        assert_eq!(object.get().bytes, &bytes[..]);
        // Several outputs of one checkpoint can be versions of one object:
        // the live one is the last written.
        let last = ids.iter().rposition(|other| other == id).unwrap();
        if last == i {
            assert_eq!(
                store.live(id).unwrap(),
                Some(Live {
                    version: *version,
                    digest: *digest
                })
            );
            assert_eq!(
                store.live_object(id).unwrap().unwrap().get().bytes,
                &bytes[..]
            );
        }
        assert!(store.object(id, version + 1_000_000).unwrap().is_none());
    }
    // Removed: no live version, every version still readable.
    let (id, version) = (ids[0], expected[0].1);
    store
        .commit(Commit {
            removed: vec![id],
            ..Commit::default()
        })
        .unwrap();
    assert_eq!(store.live(&id).unwrap(), None);
    assert!(store.live_object(&id).unwrap().is_none());
    assert!(store.object(&id, version).unwrap().is_some());
}

#[test]
fn executed_transactions_round_trip_and_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let checkpoint = checkpoint();
    let tx = &checkpoint.get().transactions[1];
    let digest = *tx.transaction.digest();
    let effects_digest = Digest::new([7; 32]);
    let events = tx.events.map(|e| e.bytes.to_vec());
    {
        let store = Store::open(dir.path()).unwrap();
        assert!(store.executed_effects(&digest).unwrap().is_none());
        store
            .commit(Commit {
                written: written(checkpoint.get()),
                removed: vec![],
                executed: Some(Executed {
                    digest,
                    transaction: tx.transaction.bytes().to_vec(),
                    effects_digest,
                    effects: tx.effects.bytes.to_vec(),
                    events: events.clone(),
                }),
            })
            .unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.executed_effects(&digest).unwrap(),
        Some(effects_digest)
    );
    let transaction = store.transaction(&digest).unwrap().unwrap();
    assert_eq!(transaction.get().digest(), &digest);
    let effects = store.effects(&effects_digest).unwrap().unwrap();
    assert_eq!(effects.get().bytes, tx.effects.bytes);
    assert_eq!(
        store
            .events(&digest)
            .unwrap()
            .map(|e| e.get().bytes.to_vec()),
        events
    );
    // And the objects committed with it.
    let object = &tx.output_objects[0];
    assert_eq!(
        store.live(object.id().unwrap()).unwrap().map(|l| l.version),
        written(checkpoint.get())
            .iter()
            .rev()
            .find(|w| Some(&w.id) == object.id())
            .map(|w| w.version)
    );
}

/// Versions of one object, written as if by successive transactions; and a
/// neighbour in id order, which must not be found in its place. Each
/// version holds a different object's bytes, to tell them apart.
#[test]
fn the_version_at_or_before_a_bound() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let checkpoint = checkpoint();
    let objects: Vec<&[u8]> = checkpoint
        .get()
        .transactions
        .iter()
        .flat_map(|tx| tx.output_objects.iter())
        .map(|o| o.bytes)
        .take(4)
        .collect();
    let id = messages::base::ObjectId([0x42; 32]);
    let next = messages::base::ObjectId([0x43; 32]);
    let write = |id, version, bytes: &[u8]| Written {
        id,
        version,
        digest: Digest::new([version as u8; 32]),
        bytes: bytes.to_vec(),
    };
    store
        .commit(Commit {
            written: vec![
                write(id, 3, objects[0]),
                write(id, 7, objects[1]),
                write(id, 12, objects[2]),
                write(next, 5, objects[3]),
            ],
            ..Commit::default()
        })
        .unwrap();
    let found = |bound| {
        store
            .object_at_or_before(&id, bound)
            .unwrap()
            .map(|o| o.get().bytes.to_vec())
    };
    assert_eq!(found(2), None);
    for (bound, expected) in [
        (3, 0),
        (6, 0),
        (7, 1),
        (11, 1),
        (12, 2),
        (100, 2),
        (u64::MAX, 2),
    ] {
        assert_eq!(
            found(bound).as_deref(),
            Some(objects[expected]),
            "bound {bound}"
        );
    }
    let before = messages::base::ObjectId([0x41; 32]);
    assert_eq!(
        store
            .object_at_or_before(&before, u64::MAX)
            .unwrap()
            .map(|_| ()),
        None
    );
}
