// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store over a real database.

use messages::Message;
use messages::base::{Digest, ObjectId, ObjectRef};
use messages::checkpoint::CheckpointData;
use store::{Commit, Live, Store, Written};
use validator::epoch_store::{ConsensusOutput, EpochStore};
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
        let (store, _, _) = EpochStore::open(&epochs, 0);
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
    let (store, _, _) = EpochStore::open(&epochs, 0);
    assert_eq!(store.start_version(&objects, &id, 1), Some(at(3)));
    let (next, _, _) = EpochStore::open(&epochs, 1);
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

/// A generation is read back at once, and reaches the epoch store when its
/// producer commits, or at once if it has: a restart finds only those.
#[test]
fn generations_are_written_once_their_producer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let table = |dir: &tempfile::TempDir| {
        let (store, _, _) = EpochStore::open(dir.path(), 0);
        GenerationTable::new(store)
    };
    let ended = Generation::Ended {
        version: 10,
        transaction: [7; 32],
    };
    {
        let table = table(&dir);
        table.record(stream(1), 3, 1, version(2), [1; 32], false);
        table.record(stream(2), 7, 1, ended, [2; 32], false);
        table.record(stream(3), 4, 1, version(5), [3; 32], true);
        assert_eq!(table.get(stream(1), 3, 1), Some(version(2)));
        assert_eq!(table.get(stream(2), 7, 1), Some(ended));
        assert_eq!(table.get(stream(1), 3, 2), None);
        table.commit(&[[1; 32]], &containers::Bump::default());
        assert_eq!(table.get(stream(1), 3, 1), Some(version(2)));
        // A stream's start, recorded once.
        assert_eq!(
            table.get_or_record(stream(4), 3, 0, || version(8)),
            version(8)
        );
        assert_eq!(
            table.get_or_record(stream(4), 3, 0, || version(99)),
            version(8)
        );
    }
    // Reopened, as on restart.
    let table = table(&dir);
    assert_eq!(table.get(stream(1), 3, 1), Some(version(2)));
    assert_eq!(table.get(stream(2), 7, 1), None);
    assert_eq!(table.get(stream(3), 4, 1), Some(version(5)));
    assert_eq!(table.get(stream(4), 3, 0), Some(version(8)));
}

fn object(n: u8) -> ObjectRef {
    ObjectRef {
        id: ObjectId([n; 32]),
        version: messages::base::U64Le::new(1),
        digest: Digest::new([n; 32]),
    }
}

/// One commit's output: `digest` processed, holding `object`'s lock, `s`
/// at `last`, then ended with record `[record]`.
fn commit(
    output: &mut ConsensusOutput,
    record: u8,
    digest: u8,
    lock: Option<u8>,
    (s, last): (u8, (u32, u32)),
    height: u64,
    drained: bool,
) {
    output.set_processed([digest; 32]);
    if let Some(n) = lock {
        output.set_lock(object(n), [digest; 32]);
    }
    output.set_stream(stream(s), last);
    output.finish_commit(vec![record], height, drained);
}

/// Each commit's output is read back at once, and written once its
/// checkpoints have committed, through the last commit that left no chunk
/// waiting: a restart finds only what was written, and the last record.
#[test]
fn consensus_output_is_written_once_its_checkpoints_commit() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (_, mut output, _) = EpochStore::open(dir.path(), 0);
        let committed = output.committed_checkpoints();
        commit(&mut output, 1, 1, Some(1), (1, (1, 1)), 10, true);
        // Not drained: a chunk of commit 2 waits for a checkpoint.
        commit(&mut output, 2, 2, Some(2), (1, (2, 1)), 11, false);
        commit(&mut output, 3, 3, None, (2, (3, 1)), 12, true);
        for n in 1..=3 {
            assert!(output.is_processed(&[n; 32]));
        }
        assert_eq!(output.lock_holder(&object(2)), Some([2; 32]));
        assert_eq!(output.stream(&stream(1)), Some((2, 1)));
        assert_eq!(output.record(), None);

        // Through height 11: commit 1 is written, commit 2 not, as it left
        // a chunk waiting.
        committed.committed_through(11);
        assert_eq!(output.record(), Some(vec![1]));
        // Stream 1, written by commit 1 and again by commit 2, reads as
        // commit 2 left it.
        assert_eq!(output.stream(&stream(1)), Some((2, 1)));
        assert!(output.is_processed(&[2; 32]));
    }
    {
        // A restart finds commit 1's output only.
        let (_, mut output, _) = EpochStore::open(dir.path(), 0);
        assert_eq!(output.record(), Some(vec![1]));
        assert!(output.is_processed(&[1; 32]));
        assert!(!output.is_processed(&[2; 32]));
        assert_eq!(output.lock_holder(&object(1)), Some([1; 32]));
        assert_eq!(output.lock_holder(&object(2)), None);
        assert_eq!(output.stream(&stream(1)), Some((1, 1)));
        assert_eq!(output.stream(&stream(2)), None);

        // Replayed, and committed through height 12: both written, as
        // commit 3 drained the queue.
        let committed = output.committed_checkpoints();
        commit(&mut output, 2, 2, Some(2), (1, (2, 1)), 11, false);
        commit(&mut output, 3, 3, None, (2, (3, 1)), 12, true);
        committed.committed_through(12);
        assert_eq!(output.record(), Some(vec![3]));
    }
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    assert!(output.is_processed(&[3; 32]));
    assert_eq!(output.lock_holder(&object(2)), Some([2; 32]));
    assert_eq!(output.stream(&stream(1)), Some((2, 1)));
    assert_eq!(output.stream(&stream(2)), Some((3, 1)));
}

/// After a restart, a commit at or below the committed checkpoints is
/// written as soon as it ends, as the reference flushes when output is
/// pushed.
#[test]
fn replayed_output_below_the_committed_checkpoints_is_written_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut output, _) = EpochStore::open(dir.path(), 0);
    output.committed_checkpoints().committed_through(20);
    commit(&mut output, 1, 1, None, (1, (1, 1)), 15, true);
    assert_eq!(output.record(), Some(vec![1]));
    commit(&mut output, 2, 2, None, (1, (2, 1)), 25, true);
    assert_eq!(output.record(), Some(vec![1]));
}
