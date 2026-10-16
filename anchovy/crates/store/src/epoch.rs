// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! An epoch's database, discarded once the epoch is over: after the
//! reference's `AuthorityEpochTables`. Raw bytes by keyspace; the pipeline's
//! epoch store gives them types and decides who writes which.
//!
//! Each epoch has a directory of two Tidehunter databases:
//! - `replayed`: what replaying the epoch's commits from its start writes
//!   again (consensus output, checkpoint builder state, settled chunks,
//!   generations), so a restart drops it;
//! - `kept`: what a replay could not rebuild, as the stores it was derived
//!   from have moved on (start versions).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tidehunter::batch::WriteBatch;
use tidehunter::config::Config;
use tidehunter::db::{Db as Tidehunter, DbError};
use tidehunter::key_shape::{KeyShapeBuilder, KeySpace, KeyType};
use tidehunter::metrics::Metrics;

use crate::Result;

/// The keyspaces, by the section of the epoch store they belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    // Consensus output.
    Processed,
    Locks,
    Streams,
    // Checkpoint builder state.
    Built,
    CheckpointHashes,
    Roots,
    // Everything else.
    Chunks,
    Generations,
    StartVersions,
}

impl Space {
    const REPLAYED: [Space; 8] = [
        Space::Processed,
        Space::Locks,
        Space::Streams,
        Space::Built,
        Space::CheckpointHashes,
        Space::Roots,
        Space::Chunks,
        Space::Generations,
    ];
    const KEPT: [Space; 1] = [Space::StartVersions];

    fn name(self) -> &'static str {
        match self {
            Space::Processed => "processed",
            Space::Locks => "locks",
            Space::Streams => "streams",
            Space::Built => "built",
            Space::CheckpointHashes => "checkpoint_hashes",
            Space::Chunks => "chunks",
            Space::Roots => "roots",
            Space::Generations => "generations",
            Space::StartVersions => "start_versions",
        }
    }

    /// Keys are fixed-length: digests and object ids are 32 bytes, versions
    /// and sequence numbers 8.
    fn key_len(self) -> usize {
        match self {
            Space::Processed => 32,
            Space::Locks => 32 + 8 + 32,
            Space::Streams | Space::StartVersions => 32 + 8,
            Space::Generations => 32 + 8 + 4 + 4,
            Space::Built | Space::CheckpointHashes | Space::Roots | Space::Chunks => 8,
        }
    }

    fn replayed(self) -> bool {
        Space::REPLAYED.contains(&self)
    }
}

/// The reference's defaults.
const MUTEXES: usize = if cfg!(debug_assertions) { 16 } else { 1024 };

const PREFIX: &str = "epoch_";

pub struct EpochDb {
    epoch: u64,
    replayed: Arc<Tidehunter>,
    kept: Arc<Tidehunter>,
    /// By `Space` discriminant.
    spaces: Vec<KeySpace>,
}

impl EpochDb {
    /// Epoch `epoch`'s database under `root`, its replayed part emptied:
    /// opened at the epoch's start, and again on restart, when the epoch
    /// replays from its start.
    pub fn open(root: &Path, epoch: u64) -> Result<EpochDb> {
        let dir = epoch_dir(root, epoch);
        let replayed_dir = dir.join("replayed");
        if replayed_dir.exists() {
            Tidehunter::drop_db(&replayed_dir).map_err(DbError::Io)?;
        }
        let replayed = open(&replayed_dir, &Space::REPLAYED)?;
        let kept = open(&dir.join("kept"), &Space::KEPT)?;
        let all = [Space::REPLAYED.as_slice(), Space::KEPT.as_slice()].concat();
        let mut spaces = vec![None; all.len()];
        for space in all {
            let db = if space.replayed() { &replayed } else { &kept };
            spaces[space as usize] = Some(db.ks(space.name()));
        }
        Ok(EpochDb {
            epoch,
            replayed,
            kept,
            spaces: spaces
                .into_iter()
                .map(|s| s.expect("every space"))
                .collect(),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    fn db(&self, space: Space) -> &Arc<Tidehunter> {
        if space.replayed() {
            &self.replayed
        } else {
            &self.kept
        }
    }

    pub fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db(space)
            .get(self.spaces[space as usize], key)?
            .map(|v| v.to_vec()))
    }

    pub fn exists(&self, space: Space, key: &[u8]) -> Result<bool> {
        Ok(self.db(space).exists(self.spaces[space as usize], key)?)
    }

    /// A batch over `spaces`, which must all be in one of the two
    /// databases: a batch commits atomically within one.
    ///
    /// # Panics
    /// If `spaces` span both databases.
    pub fn batch(&self, spaces: &[Space]) -> EpochBatch<'_> {
        let replayed = spaces.first().is_none_or(|s| s.replayed());
        assert!(
            spaces.iter().all(|s| s.replayed() == replayed),
            "a batch is within one database"
        );
        let db = if replayed { &self.replayed } else { &self.kept };
        EpochBatch {
            db: self,
            replayed,
            batch: db.write_batch(),
        }
    }
}

/// Writes to one of an epoch's databases, committed atomically.
pub struct EpochBatch<'a> {
    db: &'a EpochDb,
    replayed: bool,
    batch: WriteBatch,
}

impl EpochBatch<'_> {
    pub fn put(&mut self, space: Space, key: Vec<u8>, value: Vec<u8>) {
        assert_eq!(
            space.replayed(),
            self.replayed,
            "{space:?} is not in this batch's database"
        );
        self.batch.write(self.db.spaces[space as usize], key, value);
    }

    pub fn delete(&mut self, space: Space, key: Vec<u8>) {
        assert_eq!(
            space.replayed(),
            self.replayed,
            "{space:?} is not in this batch's database"
        );
        self.batch.delete(self.db.spaces[space as usize], key);
    }

    pub fn commit(self) -> Result<()> {
        Ok(self.batch.commit()?)
    }
}

fn open(path: &Path, spaces: &[Space]) -> Result<Arc<Tidehunter>> {
    std::fs::create_dir_all(path).map_err(DbError::Io)?;
    let mut shape = KeyShapeBuilder::new();
    for space in spaces {
        shape.add_key_space(space.name(), space.key_len(), MUTEXES, KeyType::uniform(1));
    }
    Ok(Tidehunter::open(
        path,
        shape.build(),
        Arc::new(Config::default()),
        Metrics::new(),
    )?)
}

fn epoch_dir(root: &Path, epoch: u64) -> PathBuf {
    root.join(format!("{PREFIX}{epoch}"))
}

/// Deletes the databases of epochs before `epoch - 1` under `root`: the
/// current epoch's and the previous one's stay, as the reference keeps a
/// few.
pub fn prune_before(root: &Path, epoch: u64) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.map_err(DbError::Io)?;
        let name = entry.file_name();
        let Some(old) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PREFIX))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if old + 1 < epoch {
            std::fs::remove_dir_all(entry.path()).map_err(DbError::Io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopening_drops_the_replayed_part_and_keeps_the_rest() {
        let root = tempfile::tempdir().unwrap();
        {
            let db = EpochDb::open(root.path(), 3).unwrap();
            let mut batch = db.batch(&[Space::Processed]);
            batch.put(Space::Processed, vec![1; 32], vec![]);
            batch.commit().unwrap();
            let mut batch = db.batch(&[Space::StartVersions]);
            batch.put(Space::StartVersions, vec![2; 40], vec![7]);
            batch.commit().unwrap();
            assert!(db.exists(Space::Processed, &[1; 32]).unwrap());
        }
        let db = EpochDb::open(root.path(), 3).unwrap();
        assert!(!db.exists(Space::Processed, &[1; 32]).unwrap());
        assert_eq!(
            db.get(Space::StartVersions, &[2; 40]).unwrap(),
            Some(vec![7])
        );
    }

    #[test]
    #[should_panic(expected = "a batch is within one database")]
    fn a_batch_cannot_span_both_databases() {
        let root = tempfile::tempdir().unwrap();
        let db = EpochDb::open(root.path(), 0).unwrap();
        let _ = db.batch(&[Space::Locks, Space::StartVersions]);
    }

    #[test]
    fn pruning_keeps_the_current_and_previous_epochs() {
        let root = tempfile::tempdir().unwrap();
        for epoch in 0..4 {
            drop(EpochDb::open(root.path(), epoch).unwrap());
        }
        prune_before(root.path(), 3).unwrap();
        let left: Vec<bool> = (0..4).map(|e| epoch_dir(root.path(), e).exists()).collect();
        assert_eq!(left, [false, false, true, true]);
    }
}
