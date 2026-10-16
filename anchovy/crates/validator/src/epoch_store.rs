// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store: what the pipeline keeps for one epoch, discarded once
//! it is over (`docs/two_stores_design.md`). Over the epoch's database
//! (`store::epoch`), in three sections: consensus output, written only by
//! the commit handler; checkpoint builder state; and everything else.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use messages::base::{Digest, ObjectId, ObjectRef};
use store::Live;
use store::epoch::{EpochDb, Space};

use crate::pipeline::keys::{StreamId, TxDigest};

/// Readers of every section, and the writes of the last. Cloned handles
/// share the database.
#[derive(Clone)]
pub struct EpochStore {
    db: Arc<EpochDb>,
    /// Makes `start_version`'s read and initialization one step.
    start_versions: Arc<Mutex<()>>,
}

impl EpochStore {
    /// Epoch `epoch`'s store under `root`, what a replay writes again
    /// emptied: at the epoch's start, and on restart. With it, the one
    /// writer of its consensus output, for the commit handler.
    ///
    /// # Panics
    /// If the database cannot be opened.
    pub fn open(root: &Path, epoch: u64) -> (EpochStore, ConsensusOutput) {
        let db = Arc::new(EpochDb::open(root, epoch).expect("the epoch's database opens"));
        let output = ConsensusOutput {
            db: db.clone(),
            pending: Pending::default(),
        };
        let store = EpochStore {
            db,
            start_versions: Arc::new(Mutex::new(())),
        };
        (store, output)
    }

    pub fn epoch(&self) -> u64 {
        self.db.epoch()
    }

    /// The version and digest version assignment starts shared object `id`
    /// (shared at `initial_shared_version`) at in the epoch: as recorded the
    /// first time a version was assigned to it in the epoch, else its live
    /// version in `objects`, recorded now. None if it has neither.
    ///
    /// The live version is right only before anything assigned in the epoch
    /// has written the object, which the first assignment is. It is recorded
    /// durably at once, so before any such write can commit: a replay of the
    /// epoch finds it, though the object store has moved on.
    ///
    /// # Panics
    /// If a database cannot be read or written.
    pub fn start_version(
        &self,
        objects: &store::Store,
        id: &ObjectId,
        initial_shared_version: u64,
    ) -> Option<Live> {
        let _initializing = self
            .start_versions
            .lock()
            .expect("the lock is not poisoned");
        let key = stream_key(id, initial_shared_version);
        if let Some(value) = self
            .db
            .get(Space::StartVersions, &key)
            .expect("the epoch's database reads")
        {
            return Some(Live {
                version: u64::from_le_bytes(value[..8].try_into().expect("eight bytes")),
                digest: Digest::new(value[8..].try_into().expect("a digest")),
            });
        }
        let live = objects.live(id).expect("the store reads")?;
        let mut value = live.version.to_le_bytes().to_vec();
        value.extend_from_slice(&live.digest.bytes);
        let mut batch = self.db.batch(&[Space::StartVersions]);
        batch.put(Space::StartVersions, key.to_vec(), value);
        batch.commit().expect("the epoch's database writes");
        Some(live)
    }
}

/// A stream's key: its object id, then the big-endian version its sharing
/// started at.
fn stream_key(id: &ObjectId, initial_shared_version: u64) -> [u8; 40] {
    let mut key = [0; 40];
    key[..32].copy_from_slice(&id.0);
    key[32..].copy_from_slice(&initial_shared_version.to_be_bytes());
    key
}

/// The epoch's consensus output: what the commit handler decided, written
/// by it alone, as this writer is made once, with the store, and cannot be
/// cloned. Read back only by the commit handler too.
///
/// Writes since the last `commit` are pending: read back from memory, and
/// written together by `commit`, which the handler calls once per commit.
pub struct ConsensusOutput {
    db: Arc<EpochDb>,
    pending: Pending,
}

#[derive(Default)]
struct Pending {
    processed: HashSet<TxDigest>,
    locks: HashMap<ObjectRef, TxDigest>,
    streams: HashMap<StreamId, (u32, u32)>,
}

impl ConsensusOutput {
    /// Whether the user transaction with `digest` was processed this epoch.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn is_processed(&self, digest: &TxDigest) -> bool {
        self.pending.processed.contains(digest)
            || self
                .db
                .exists(Space::Processed, digest)
                .expect("the epoch's database reads")
    }

    pub fn set_processed(&mut self, digest: TxDigest) {
        self.pending.processed.insert(digest);
    }

    /// The transaction holding owned reference `object`'s lock, if any.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn lock_holder(&self, object: &ObjectRef) -> Option<TxDigest> {
        if let Some(holder) = self.pending.locks.get(object) {
            return Some(*holder);
        }
        self.db
            .get(Space::Locks, &lock_key(object))
            .expect("the epoch's database reads")
            .map(|v| v[..].try_into().expect("a 32-byte digest"))
    }

    pub fn set_lock(&mut self, object: ObjectRef, holder: TxDigest) {
        self.pending.locks.insert(object, holder);
    }

    /// The stream's last scheduled generation, (round, generation), if a
    /// round scheduled it this epoch.
    ///
    /// # Panics
    /// If the database cannot be read.
    pub fn stream(&self, stream: &StreamId) -> Option<(u32, u32)> {
        if let Some(last) = self.pending.streams.get(stream) {
            return Some(*last);
        }
        self.db
            .get(
                Space::Streams,
                &stream_key(&stream.id, stream.initial_shared_version),
            )
            .expect("the epoch's database reads")
            .map(|v| {
                let word = |i: usize| u32::from_le_bytes(v[i..i + 4].try_into().expect("4 bytes"));
                (word(0), word(4))
            })
    }

    pub fn set_stream(&mut self, stream: StreamId, last: (u32, u32)) {
        self.pending.streams.insert(stream, last);
    }

    /// Writes everything pending, in one batch.
    ///
    /// # Panics
    /// If the database cannot be written.
    pub fn commit(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let mut batch = self
            .db
            .batch(&[Space::Processed, Space::Locks, Space::Streams]);
        for digest in pending.processed {
            batch.put(Space::Processed, digest.to_vec(), Vec::new());
        }
        for (object, holder) in pending.locks {
            batch.put(Space::Locks, lock_key(&object).to_vec(), holder.to_vec());
        }
        for (stream, (round, generation)) in pending.streams {
            let mut value = round.to_le_bytes().to_vec();
            value.extend_from_slice(&generation.to_le_bytes());
            batch.put(
                Space::Streams,
                stream_key(&stream.id, stream.initial_shared_version).to_vec(),
                value,
            );
        }
        batch.commit().expect("the epoch's database writes");
    }
}

/// An owned reference's key: its id, big-endian version and digest.
fn lock_key(object: &ObjectRef) -> [u8; 72] {
    let mut key = [0; 72];
    key[..32].copy_from_slice(&object.id.0);
    key[32..40].copy_from_slice(&object.version.get().to_be_bytes());
    key[40..].copy_from_slice(&object.digest.bytes);
    key
}
