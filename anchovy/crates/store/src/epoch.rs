// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! An epoch's database, discarded once the epoch is over: after the
//! reference's `AuthorityEpochTables`. Raw bytes by keyspace; the pipeline's
//! epoch store gives them types and decides who writes which. A directory
//! per epoch, holding the epoch store's Tidehunter database (`store`) and
//! consensus's (`consensus`).

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
    Commit,
    ExecutionTimeObservations,
    Deferred,
    JwkVotes,
    // Checkpoint builder state.
    Built,
    CheckpointHashes,
    Roots,
    // Everything else.
    Generations,
    StartVersions,
}

impl Space {
    const ALL: [Space; 12] = [
        Space::Processed,
        Space::Locks,
        Space::Streams,
        Space::Commit,
        Space::ExecutionTimeObservations,
        Space::Deferred,
        Space::JwkVotes,
        Space::Built,
        Space::CheckpointHashes,
        Space::Roots,
        Space::Generations,
        Space::StartVersions,
    ];

    fn name(self) -> &'static str {
        match self {
            Space::Processed => "processed",
            Space::Locks => "locks",
            Space::Streams => "streams",
            Space::Commit => "commit",
            Space::ExecutionTimeObservations => "execution_time_observations",
            Space::Deferred => "deferred",
            Space::JwkVotes => "jwk_votes",
            Space::Built => "built",
            Space::CheckpointHashes => "checkpoint_hashes",
            Space::Roots => "roots",
            Space::Generations => "generations",
            Space::StartVersions => "start_versions",
        }
    }

    /// Keys are fixed-length: digests and object ids are 32 bytes, versions
    /// and sequence numbers 8.
    fn key_len(self) -> usize {
        match self {
            // A digest, or for a JSON web key vote, its hash.
            Space::Processed | Space::JwkVotes => 32,
            Space::Locks => 32 + 8 + 32,
            Space::Streams | Space::StartVersions => 32 + 8,
            Space::Generations => 32 + 8 + 4 + 4,
            Space::Built | Space::CheckpointHashes | Space::Roots => 8,
            Space::Commit => 1,
            // A generation and an authority's index.
            Space::ExecutionTimeObservations => 8 + 4,
            // A deferral key: its variant, then two big-endian rounds.
            Space::Deferred => 1 + 8 + 8,
        }
    }
}

/// The reference's defaults.
const MUTEXES: usize = if cfg!(debug_assertions) { 16 } else { 1024 };

const PREFIX: &str = "epoch_";

pub struct EpochDb {
    epoch: u64,
    db: Arc<Tidehunter>,
    /// By `Space` discriminant.
    spaces: Vec<KeySpace>,
}

impl EpochDb {
    /// Epoch `epoch`'s database under `root`: made at the epoch's start,
    /// reopened as it was on restart.
    pub fn open(root: &Path, epoch: u64) -> Result<EpochDb> {
        let path = epoch_dir(root, epoch).join("store");
        std::fs::create_dir_all(&path).map_err(DbError::Io)?;
        let mut shape = KeyShapeBuilder::new();
        for space in Space::ALL {
            shape.add_key_space(space.name(), space.key_len(), MUTEXES, KeyType::uniform(1));
        }
        let db = Tidehunter::open(
            &path,
            shape.build(),
            Arc::new(Config::default()),
            Metrics::new(),
        )?;
        let mut spaces = vec![None; Space::ALL.len()];
        for space in Space::ALL {
            spaces[space as usize] = Some(db.ks(space.name()));
        }
        Ok(EpochDb {
            epoch,
            db,
            spaces: spaces
                .into_iter()
                .map(|s| s.expect("every space"))
                .collect(),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .get(self.spaces[space as usize], key)?
            .map(|v| v.to_vec()))
    }

    pub fn exists(&self, space: Space, key: &[u8]) -> Result<bool> {
        Ok(self.db.exists(self.spaces[space as usize], key)?)
    }

    /// The keys of `space`, whose keys are big-endian numbers, above `n`.
    pub fn numbers_above(&self, space: Space, n: u64) -> Result<Vec<u64>> {
        let mut entries = self.db.iterator(self.spaces[space as usize]);
        entries.reverse();
        let mut above = Vec::new();
        for entry in entries {
            let (key, _) = entry?;
            let key = u64::from_be_bytes(key[..8].try_into().expect("an 8-byte key"));
            if key <= n {
                break;
            }
            above.push(key);
        }
        Ok(above)
    }

    /// Every entry of `space`, in key order: for reading state back at
    /// start-up.
    pub fn entries(&self, space: Space) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.db
            .iterator(self.spaces[space as usize])
            .map(|entry| {
                let (key, value) = entry?;
                Ok((key.to_vec(), value.to_vec()))
            })
            .collect()
    }

    /// Writes, committed atomically.
    pub fn batch(&self) -> EpochBatch<'_> {
        EpochBatch {
            db: self,
            batch: self.db.write_batch(),
        }
    }
}

/// Writes to an epoch's database, committed atomically.
pub struct EpochBatch<'a> {
    db: &'a EpochDb,
    batch: WriteBatch,
}

impl EpochBatch<'_> {
    pub fn put(&mut self, space: Space, key: Vec<u8>, value: Vec<u8>) {
        self.batch.write(self.db.spaces[space as usize], key, value);
    }

    pub fn delete(&mut self, space: Space, key: Vec<u8>) {
        self.batch.delete(self.db.spaces[space as usize], key);
    }

    pub fn commit(self) -> Result<()> {
        Ok(self.batch.commit()?)
    }
}

/// Where epoch `epoch`'s consensus keeps its data under `root`: in the
/// epoch's directory, so it goes with the epoch's store.
pub fn consensus_dir(root: &Path, epoch: u64) -> PathBuf {
    epoch_dir(root, epoch).join("consensus")
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
    fn reopening_keeps_everything() {
        let root = tempfile::tempdir().unwrap();
        {
            let db = EpochDb::open(root.path(), 3).unwrap();
            let mut batch = db.batch();
            batch.put(Space::Processed, vec![1; 32], vec![]);
            batch.put(Space::StartVersions, vec![2; 40], vec![7]);
            batch.commit().unwrap();
        }
        let db = EpochDb::open(root.path(), 3).unwrap();
        assert!(db.exists(Space::Processed, &[1; 32]).unwrap());
        assert_eq!(
            db.get(Space::StartVersions, &[2; 40]).unwrap(),
            Some(vec![7])
        );
    }

    #[test]
    fn numbers_above_are_found_from_the_top() {
        let root = tempfile::tempdir().unwrap();
        let db = EpochDb::open(root.path(), 0).unwrap();
        let mut batch = db.batch();
        for n in [3u64, 5, 300, 7] {
            batch.put(Space::Roots, n.to_be_bytes().to_vec(), vec![]);
        }
        batch.commit().unwrap();
        assert_eq!(db.numbers_above(Space::Roots, 5).unwrap(), [300, 7]);
        assert!(db.numbers_above(Space::Roots, 300).unwrap().is_empty());
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
