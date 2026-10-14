// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The database: objects, and executed transactions with their effects and
//! events, in one Tidehunter database. After the reference's
//! `AuthorityPerpetualTables`, with only the tables used now. Values are the
//! reference's BCS, read back as zero-copy views. Reached only through the
//! cache (`Store`).

use std::path::Path;
use std::sync::Arc;

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::effects::{TransactionEffects, TransactionEvents};
use messages::object::Object;
use messages::transaction::SenderSignedData;
use tidehunter::batch::WriteBatch;
use tidehunter::config::Config;
use tidehunter::db::{Db as Tidehunter, DbError};
use tidehunter::key_shape::{KeyShapeBuilder, KeySpace, KeyType};
use tidehunter::metrics::Metrics;

use crate::{Commit, Error, Live, Removal, Result};

/// An object's latest version in the database.
pub(crate) enum Latest {
    Object(Message<Object<'static>>, Digest),
    Deleted,
    Wrapped,
}

/// A tombstone's value in `objects`: one byte, where an object's encoding is always longer.
const DELETED: &[u8] = &[0];
const WRAPPED: &[u8] = &[1];

pub(crate) struct Db {
    tidehunter: Arc<Tidehunter>,
    /// Object id and big-endian version: every version of an object, in
    /// order.
    objects: KeySpace,
    /// Object id: the live version and its digest. Stands in for finding
    /// the latest key in `objects`.
    live: KeySpace,
    transactions: KeySpace,
    effects: KeySpace,
    /// Transaction digest: effects digest.
    executed_effects: KeySpace,
    /// Transaction digest: events.
    events: KeySpace,
    /// Facts about the database itself: whether genesis is in.
    meta: KeySpace,
    /// Big-endian sequence number: each checkpoint this node has executed,
    /// as its pipeline encodes it. The highest is the executed watermark.
    checkpoints: KeySpace,
}

const ID: usize = 32;
const DIGEST: usize = 32;
/// `meta`'s keys.
const GENESIS: &[u8; 8] = b"genesis\0";
const COMMITTED: &[u8; 8] = b"commitwm";

/// The reference's defaults.
const MUTEXES: usize = if cfg!(debug_assertions) { 16 } else { 1024 };

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        std::fs::create_dir_all(path).map_err(DbError::Io)?;
        let uniform = KeyType::uniform(1);
        let mut shape = KeyShapeBuilder::new();
        shape
            .add_key_space("objects", ID + 8, MUTEXES, uniform)
            .add_key_space("live_objects", ID, MUTEXES, uniform)
            .add_key_space("transactions", DIGEST, MUTEXES, uniform)
            .add_key_space("effects", DIGEST, MUTEXES, uniform)
            .add_key_space("executed_effects", DIGEST, MUTEXES, uniform)
            .add_key_space("events", DIGEST, MUTEXES, uniform)
            .add_key_space("meta", GENESIS.len(), 1, uniform)
            .add_key_space("checkpoints", 8, MUTEXES, uniform);
        let db = Tidehunter::open(
            path,
            shape.build(),
            Arc::new(Config::default()),
            Metrics::new(),
        )?;
        Ok(Db {
            objects: db.ks("objects"),
            live: db.ks("live_objects"),
            transactions: db.ks("transactions"),
            effects: db.ks("effects"),
            executed_effects: db.ks("executed_effects"),
            events: db.ks("events"),
            meta: db.ks("meta"),
            checkpoints: db.ks("checkpoints"),
            tidehunter: db,
        })
    }

    /// Whether genesis is in. (Not `Db::is_empty`, which lags commits:
    /// Tidehunter indexes a batch in the background.)
    pub fn has_genesis(&self) -> Result<bool> {
        Ok(self.tidehunter.exists(self.meta, GENESIS)?)
    }

    /// Genesis: its objects, and the mark `has_genesis` reads, together.
    pub fn commit_genesis(&self, objects: Vec<crate::Written>) -> Result<()> {
        let mut batch = self.batch(Commit {
            written: objects,
            ..Commit::default()
        });
        batch.write(self.meta, GENESIS.to_vec(), Vec::new());
        Ok(batch.commit()?)
    }

    /// Records executed checkpoint `seq`, encoded.
    pub fn put_checkpoint(&self, seq: u64, checkpoint: Vec<u8>) -> Result<()> {
        let mut batch = self.tidehunter.write_batch();
        batch.write(self.checkpoints, seq.to_be_bytes().to_vec(), checkpoint);
        Ok(batch.commit()?)
    }

    pub fn checkpoint(&self, seq: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .tidehunter
            .get(self.checkpoints, &seq.to_be_bytes())?
            .map(|v| v.to_vec()))
    }

    /// The highest executed checkpoint, and its sequence number: none on a
    /// fresh database.
    pub fn last_checkpoint(&self) -> Result<Option<(u64, Vec<u8>)>> {
        let mut checkpoints = self.tidehunter.iterator(self.checkpoints);
        checkpoints.reverse();
        match checkpoints.next() {
            Some(entry) => {
                let (key, value) = entry?;
                let seq = u64::from_be_bytes(key[..8].try_into().expect("an 8-byte key"));
                Ok(Some((seq, value.to_vec())))
            }
            None => Ok(None),
        }
    }

    /// The object at exactly `version`; none if there is none, or a tombstone.
    pub fn object(&self, id: &ObjectId, version: u64) -> Result<Option<Message<Object<'static>>>> {
        match self
            .tidehunter
            .get(self.objects, &object_key(id, version))?
        {
            Some(value) => parse_object(value.to_vec()),
            None => Ok(None),
        }
    }

    pub fn live(&self, id: &ObjectId) -> Result<Option<Live>> {
        Ok(self.tidehunter.get(self.live, &id.0)?.map(|value| {
            let version = u64::from_be_bytes(value[..8].try_into().expect("eight bytes"));
            let digest = Digest::new(value[8..].try_into().expect("thirty-two bytes"));
            Live { version, digest }
        }))
    }

    /// The object's highest version at or below `bound`, live or not; none if that is a
    /// tombstone (sui's `find_object_lt_or_eq_version`). Version `u64::MAX` is never found.
    pub fn object_at_or_before(
        &self,
        id: &ObjectId,
        bound: u64,
    ) -> Result<Option<Message<Object<'static>>>> {
        let mut versions = self.tidehunter.iterator(self.objects);
        versions.reverse();
        versions.set_lower_bound(object_key(id, 0).to_vec());
        // Exclusive; at `u64::MAX` the bound misses that version, which sui
        // reserves and never gives an object.
        versions.set_upper_bound(object_key(id, bound.saturating_add(1)).to_vec());
        match versions.next() {
            Some(entry) => {
                let (key, value) = entry?;
                if key[..ID] != id.0 {
                    return Ok(None);
                }
                // A tombstone: the object was deleted or wrapped at or below `bound`.
                parse_object(value.to_vec())
            }
            None => Ok(None),
        }
    }

    pub fn transaction(
        &self,
        digest: &Digest,
    ) -> Result<Option<Message<SenderSignedData<'static>>>> {
        self.get(self.transactions, &digest.bytes, "transactions")
    }

    /// The digest of the effects of the transaction, if it executed.
    pub fn executed_effects(&self, transaction: &Digest) -> Result<Option<Digest>> {
        Ok(self
            .tidehunter
            .get(self.executed_effects, &transaction.bytes)?
            .map(|value| Digest::new(value[..].try_into().expect("thirty-two bytes"))))
    }

    pub fn effects(&self, digest: &Digest) -> Result<Option<Message<TransactionEffects<'static>>>> {
        self.get(self.effects, &digest.bytes, "effects")
    }

    pub fn events(
        &self,
        transaction: &Digest,
    ) -> Result<Option<Message<TransactionEvents<'static>>>> {
        self.get(self.events, &transaction.bytes, "events")
    }

    pub fn commit(&self, commit: Commit) -> Result<()> {
        Ok(self.batch(commit).commit()?)
    }

    fn batch(&self, commit: Commit) -> WriteBatch {
        let mut batch = self.tidehunter.write_batch();
        for written in commit.written {
            self.put_object(
                &mut batch,
                &written.id,
                written.version,
                &written.digest,
                written.bytes,
            );
        }
        for removed in commit.removed {
            self.put_tombstone(&mut batch, &removed.id, removed.version, removed.removal);
        }
        if let Some(executed) = commit.executed {
            self.put_executed(
                &mut batch,
                &executed.digest,
                executed.transaction,
                &executed.effects_digest,
                executed.effects,
                executed.events,
            );
        }
        batch
    }

    pub fn write_batch(&self) -> WriteBatch {
        self.tidehunter.write_batch()
    }

    /// `id` at `version` becomes its live version.
    pub fn put_object(
        &self,
        batch: &mut WriteBatch,
        id: &ObjectId,
        version: u64,
        digest: &Digest,
        bytes: Vec<u8>,
    ) {
        batch.write(self.objects, object_key(id, version).to_vec(), bytes);
        let mut live = version.to_be_bytes().to_vec();
        live.extend_from_slice(&digest.bytes);
        batch.write(self.live, id.0.to_vec(), live);
    }

    /// `id` has no live version from `version` on.
    pub fn put_tombstone(
        &self,
        batch: &mut WriteBatch,
        id: &ObjectId,
        version: u64,
        removal: Removal,
    ) {
        let tombstone = match removal {
            Removal::Deleted => DELETED,
            Removal::Wrapped => WRAPPED,
        };
        batch.write(
            self.objects,
            object_key(id, version).to_vec(),
            tombstone.to_vec(),
        );
        batch.delete(self.live, id.0.to_vec());
    }

    pub fn put_executed(
        &self,
        batch: &mut WriteBatch,
        digest: &Digest,
        transaction: Vec<u8>,
        effects_digest: &Digest,
        effects: Vec<u8>,
        events: Option<Vec<u8>>,
    ) {
        let key = digest.bytes.to_vec();
        batch.write(self.transactions, key.clone(), transaction);
        batch.write(self.effects, effects_digest.bytes.to_vec(), effects);
        batch.write(
            self.executed_effects,
            key.clone(),
            effects_digest.bytes.to_vec(),
        );
        if let Some(events) = events {
            batch.write(self.events, key, events);
        }
    }

    /// Checkpoint `seq` and the committed watermark, which says the
    /// outputs of every checkpoint through `seq` are in the database.
    pub fn put_committed_checkpoint(&self, batch: &mut WriteBatch, seq: u64, checkpoint: Vec<u8>) {
        batch.write(self.checkpoints, seq.to_be_bytes().to_vec(), checkpoint);
        batch.write(self.meta, COMMITTED.to_vec(), seq.to_be_bytes().to_vec());
    }

    /// The highest checkpoint whose outputs are all in the database.
    pub fn committed_checkpoint(&self) -> Result<Option<u64>> {
        Ok(self
            .tidehunter
            .get(self.meta, COMMITTED)?
            .map(|v| u64::from_be_bytes(v[..].try_into().expect("eight bytes"))))
    }

    /// The object's latest version, live or a tombstone; none if it never
    /// existed.
    pub fn latest(&self, id: &ObjectId) -> Result<Option<(u64, Latest)>> {
        if let Some(live) = self.live(id)? {
            let object = self.object(id, live.version)?.ok_or(Error::Inconsistent(
                "a live row names a version with no object",
            ))?;
            return Ok(Some((live.version, Latest::Object(object, live.digest))));
        }
        let mut versions = self.tidehunter.iterator(self.objects);
        versions.reverse();
        versions.set_lower_bound(object_key(id, 0).to_vec());
        versions.set_upper_bound(object_key(id, u64::MAX).to_vec());
        match versions.next() {
            Some(entry) => {
                let (key, value) = entry?;
                if key[..ID] != id.0 {
                    return Ok(None);
                }
                let version = u64::from_be_bytes(key[ID..].try_into().expect("eight bytes"));
                let latest = match value.as_ref() {
                    DELETED => Latest::Deleted,
                    WRAPPED => Latest::Wrapped,
                    // Without a live row, the latest version is a tombstone.
                    _ => {
                        return Err(Error::Inconsistent(
                            "an object's latest version is live, but it has no live row",
                        ));
                    }
                };
                Ok(Some((version, latest)))
            }
            None => Ok(None),
        }
    }

    fn get<T: messages::Parse>(
        &self,
        ks: KeySpace,
        key: &[u8],
        table: &'static str,
    ) -> Result<Option<Message<T>>> {
        match self.tidehunter.get(ks, key)? {
            Some(value) => Message::parse(value.to_vec())
                .map(Some)
                .map_err(|(error, _)| Error::Corrupt { table, error }),
            None => Ok(None),
        }
    }
}

/// An `objects` value: an object, or none for a tombstone.
fn parse_object(value: Vec<u8>) -> Result<Option<Message<Object<'static>>>> {
    if value.as_slice() == DELETED || value.as_slice() == WRAPPED {
        return Ok(None);
    }
    Message::parse(value)
        .map(Some)
        .map_err(|(error, _)| Error::Corrupt {
            table: "objects",
            error,
        })
}

/// Big-endian, so that an object's versions are in order.
fn object_key(id: &ObjectId, version: u64) -> [u8; ID + 8] {
    let mut key = [0; ID + 8];
    key[..ID].copy_from_slice(&id.0);
    key[ID..].copy_from_slice(&version.to_be_bytes());
    key
}
