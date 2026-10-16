// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch store: what the pipeline keeps for one epoch, discarded once
//! it is over (`docs/two_stores_design.md`). Over the epoch's database
//! (`store::epoch`), in three sections: consensus output, written only by
//! the commit handler; checkpoint builder state; and everything else.

use std::path::Path;
use std::sync::{Arc, Mutex};

use messages::base::{Digest, ObjectId};
use store::Live;
use store::epoch::{EpochDb, Space};

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
    /// emptied: at the epoch's start, and on restart.
    ///
    /// # Panics
    /// If the database cannot be opened.
    pub fn open(root: &Path, epoch: u64) -> EpochStore {
        EpochStore {
            db: Arc::new(EpochDb::open(root, epoch).expect("the epoch's database opens")),
            start_versions: Arc::new(Mutex::new(())),
        }
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
