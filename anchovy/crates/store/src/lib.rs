// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The node's persistent state: objects, and executed transactions with
//! their effects and events, in one Tidehunter database. After the
//! reference's `AuthorityPerpetualTables`, with only the tables used now.
//! Values are the reference's BCS, read back as zero-copy views.

use std::path::Path;
use std::sync::Arc;

use messages::base::{Digest, ObjectId};
use messages::effects::{TransactionEffects, TransactionEvents};
use messages::object::Object;
use messages::transaction::SenderSignedData;
use messages::{Message, ParseError};
use tidehunter::config::Config;
use tidehunter::db::{Db, DbError};
use tidehunter::key_shape::{KeyShapeBuilder, KeySpace, KeyType};
use tidehunter::metrics::Metrics;

#[derive(Debug)]
pub enum Error {
    Db(DbError),
    /// A stored value does not parse: the database is corrupt.
    Corrupt {
        table: &'static str,
        error: ParseError,
    },
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Error {
        Error::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The version and digest of an object's live version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    pub version: u64,
    pub digest: Digest,
}

/// One new object version.
pub struct Written {
    pub id: ObjectId,
    pub version: u64,
    pub digest: Digest,
    /// `Object` BCS.
    pub bytes: Vec<u8>,
}

/// A transaction that executed, and its results, as BCS.
pub struct Executed {
    pub digest: Digest,
    /// `SenderSignedData`.
    pub transaction: Vec<u8>,
    pub effects_digest: Digest,
    /// `TransactionEffects`.
    pub effects: Vec<u8>,
    /// `TransactionEvents`, if there are any.
    pub events: Option<Vec<u8>>,
}

/// Why an object has no live version any more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The effects' `deleted` and `unwrapped_then_deleted`.
    Deleted,
    /// The effects' `wrapped`: it may come back, at a later version.
    Wrapped,
}

/// An object the transaction leaves without a live version, at the version the effects give
/// it (the transaction's lamport version).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removed {
    pub id: ObjectId,
    pub version: u64,
    pub removal: Removal,
}

/// What one transaction changes, applied atomically.
#[derive(Default)]
pub struct Commit {
    /// New object versions, each becoming its object's live version.
    pub written: Vec<Written>,
    /// Objects with no live version any more: each gets a tombstone at its version, as sui's
    /// store writes `StoreObject::Deleted`/`Wrapped`, so that reading at or below a later
    /// version finds the tombstone rather than the last version before it.
    pub removed: Vec<Removed>,
    pub executed: Option<Executed>,
}

/// A tombstone's value in `objects`: one byte, where an object's encoding is always longer.
const DELETED: &[u8] = &[0];
const WRAPPED: &[u8] = &[1];

pub struct Store {
    db: Arc<Db>,
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
}

const ID: usize = 32;
const DIGEST: usize = 32;
/// `meta`'s keys.
const GENESIS: &[u8; 8] = b"genesis\0";

/// The reference's defaults.
const MUTEXES: usize = if cfg!(debug_assertions) { 16 } else { 1024 };

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
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
            .add_key_space("meta", GENESIS.len(), 1, uniform);
        let db = Db::open(
            path,
            shape.build(),
            Arc::new(Config::default()),
            Metrics::new(),
        )?;
        Ok(Store {
            objects: db.ks("objects"),
            live: db.ks("live_objects"),
            transactions: db.ks("transactions"),
            effects: db.ks("effects"),
            executed_effects: db.ks("executed_effects"),
            events: db.ks("events"),
            meta: db.ks("meta"),
            db,
        })
    }

    /// Whether genesis is in. (Not `Db::is_empty`, which lags commits:
    /// Tidehunter indexes a batch in the background.)
    pub fn has_genesis(&self) -> Result<bool> {
        Ok(self.db.exists(self.meta, GENESIS)?)
    }

    /// Genesis: its objects, and the mark `has_genesis` reads, together.
    pub fn commit_genesis(&self, objects: Vec<Written>) -> Result<()> {
        let mut batch = self.batch(Commit {
            written: objects,
            ..Commit::default()
        });
        batch.write(self.meta, GENESIS.to_vec(), Vec::new());
        Ok(batch.commit()?)
    }

    /// The object at exactly `version`; none if there is none, or a tombstone.
    pub fn object(&self, id: &ObjectId, version: u64) -> Result<Option<Message<Object<'static>>>> {
        match self.db.get(self.objects, &object_key(id, version))? {
            Some(value) => parse_object(value.to_vec()),
            None => Ok(None),
        }
    }

    pub fn live(&self, id: &ObjectId) -> Result<Option<Live>> {
        Ok(self.db.get(self.live, &id.0)?.map(|value| {
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
        let mut versions = self.db.iterator(self.objects);
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

    /// The object's live version.
    pub fn live_object(&self, id: &ObjectId) -> Result<Option<Message<Object<'static>>>> {
        match self.live(id)? {
            Some(live) => self.object(id, live.version),
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
            .db
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

    fn batch(&self, commit: Commit) -> tidehunter::batch::WriteBatch {
        let mut batch = self.db.write_batch();
        for written in commit.written {
            batch.write(
                self.objects,
                object_key(&written.id, written.version).to_vec(),
                written.bytes,
            );
            let mut live = written.version.to_be_bytes().to_vec();
            live.extend_from_slice(&written.digest.bytes);
            batch.write(self.live, written.id.0.to_vec(), live);
        }
        for removed in commit.removed {
            let tombstone = match removed.removal {
                Removal::Deleted => DELETED,
                Removal::Wrapped => WRAPPED,
            };
            batch.write(
                self.objects,
                object_key(&removed.id, removed.version).to_vec(),
                tombstone.to_vec(),
            );
            batch.delete(self.live, removed.id.0.to_vec());
        }
        if let Some(executed) = commit.executed {
            let digest = executed.digest.bytes.to_vec();
            batch.write(self.transactions, digest.clone(), executed.transaction);
            batch.write(
                self.effects,
                executed.effects_digest.bytes.to_vec(),
                executed.effects,
            );
            batch.write(
                self.executed_effects,
                digest.clone(),
                executed.effects_digest.bytes.to_vec(),
            );
            if let Some(events) = executed.events {
                batch.write(self.events, digest, events);
            }
        }
        batch
    }

    fn get<T: messages::Parse>(
        &self,
        ks: KeySpace,
        key: &[u8],
        table: &'static str,
    ) -> Result<Option<Message<T>>> {
        match self.db.get(ks, key)? {
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
