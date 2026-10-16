// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store over a real database.

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::checkpoint::CheckpointData;
use store::{Commit, Live, Store, Written};
use validator::epoch_store::EpochStore;

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
        let store = EpochStore::open(&epochs, 0);
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
    let store = EpochStore::open(&epochs, 0);
    assert_eq!(store.start_version(&objects, &id, 1), Some(at(3)));
    let next = EpochStore::open(&epochs, 1);
    assert_eq!(next.start_version(&objects, &id, 1), Some(at(7)));
}
