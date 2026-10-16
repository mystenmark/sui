// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store over a real database.

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::checkpoint::CheckpointData;
use store::{Commit, Live, Store, Written};
use validator::epoch_store::EpochStore;
use validator::pipeline::keys::StreamId;
use validator::pipeline::tables::{Generation, GenerationTable};

/// An object's bytes, from the checked-in mainnet checkpoint.
fn object_bytes() -> Vec<u8> {
    let mut bytes = include_bytes!("../../messages/tests/data/mainnet-325300367.chk").to_vec();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes)
        .map_err(|(e, _)| e)
        .unwrap();
    checkpoint.get().transactions[0].output_objects[0]
        .bytes
        .to_vec()
}

/// A shared object's start version is its live version when first asked
/// for in an epoch, and stays that through later writes and a restart; the
/// next epoch's store starts afresh.
#[test]
fn a_start_version_is_fixed_at_the_first_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let epochs = dir.path().join("epochs");
    let bytes = object_bytes();
    let id = ObjectId([0x42; 32]);
    let write = |version: u64| Commit {
        written: vec![Written {
            id,
            version,
            digest: Digest::new([version as u8; 32]),
            bytes: bytes.clone(),
        }],
        ..Commit::default()
    };
    let at = |version: u64| Live {
        version,
        digest: Digest::new([version as u8; 32]),
    };
    let objects = Store::open(&dir.path().join("perpetual")).unwrap();
    {
        let (store, _) = EpochStore::open(&epochs, 0);
        assert_eq!(store.start_version(&objects, &id, 1), None);
        objects.write_through(write(3)).unwrap();
        assert_eq!(store.start_version(&objects, &id, 1), Some(at(3)));
        objects.write_through(write(5)).unwrap();
        assert_eq!(store.start_version(&objects, &id, 1), Some(at(3)));
        // Another stream of the object starts afresh.
        assert_eq!(store.start_version(&objects, &id, 2), Some(at(5)));
    }
    objects.write_through(write(7)).unwrap();
    // Reopened, as on restart.
    let (store, _) = EpochStore::open(&epochs, 0);
    assert_eq!(store.start_version(&objects, &id, 1), Some(at(3)));
    let (next, _) = EpochStore::open(&epochs, 1);
    assert_eq!(next.start_version(&objects, &id, 1), Some(at(7)));
}

fn stream(n: u8) -> StreamId {
    StreamId {
        id: ObjectId([n; 32]),
        initial_shared_version: 1,
    }
}

fn version(v: u64) -> Generation {
    Generation::Version {
        version: v,
        digest: [v as u8; 32],
    }
}

/// A table with stream 1 written in rounds 3 and 5, stream 2 in round 4 and
/// ended in round 7.
fn table(dir: &tempfile::TempDir) -> GenerationTable {
    let (store, _) = EpochStore::open(dir.path(), 0);
    let table = GenerationTable::new(store);
    table.record(stream(1), 3, 0, version(1));
    table.record(stream(1), 3, 1, version(2));
    table.record(stream(1), 5, 1, version(3));
    table.record(stream(2), 4, 1, version(9));
    table.record(
        stream(2),
        7,
        1,
        Generation::Ended {
            version: 10,
            transaction: [7; 32],
        },
    );
    table
}

/// Pruning keeps each stream's last generation below the round readable,
/// from the epoch store, and every generation at or above it.
#[test]
fn pruning_keeps_each_streams_last_generation() {
    let dir = tempfile::tempdir().unwrap();
    let table = table(&dir);
    table.prune_below(6);
    assert_eq!(table.get(stream(1), 5, 1), Some(version(3)));
    assert_eq!(table.get(stream(2), 4, 1), Some(version(9)));
    assert!(matches!(
        table.get(stream(2), 7, 1),
        Some(Generation::Ended { version: 10, .. })
    ));
    // Not yet recorded, at or above the pruned rounds: a reader waits.
    assert_eq!(table.get(stream(1), 6, 0), None);
    // A later prune keeps what an earlier one kept.
    table.prune_below(8);
    assert_eq!(table.get(stream(1), 5, 1), Some(version(3)));
    assert!(matches!(
        table.get(stream(2), 7, 1),
        Some(Generation::Ended { version: 10, .. })
    ));
}

#[test]
#[should_panic(expected = "was pruned, not kept")]
fn a_pruned_generation_no_reader_needs_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let table = table(&dir);
    table.prune_below(6);
    table.get(stream(1), 3, 1);
}
