// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The node's state: the database (`db`), reached only through the
//! writeback cache (`Store`, in `cache`). After the reference's
//! `AuthorityPerpetualTables` and `WritebackCache`.

mod cache;
mod db;
pub mod epoch;

pub use cache::{Capacity, Store};

use messages::ParseError;
use messages::base::{Digest, Hashed, ObjectId};
use tidehunter::db::DbError;

#[derive(Debug)]
pub enum Error {
    Db(DbError),
    /// A stored value does not parse: the database is corrupt.
    Corrupt {
        table: &'static str,
        error: ParseError,
    },
    /// Two tables disagree: the database is corrupt.
    Inconsistent(&'static str),
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
    pub digest: Hashed,
    /// `SenderSignedData`.
    pub transaction: Vec<u8>,
    pub effects_digest: Hashed,
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
