// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The keys work waits on, one type per waiter. A key is notified only once
//! the state its availability check reads holds it.

use messages::base::ObjectId;

/// A transaction digest.
pub type TxDigest = [u8; 32];

/// A shared-object stream: an object id and the version its sharing started
/// at, as the reference keys shared objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId {
    pub id: ObjectId,
    pub initial_shared_version: u64,
}

/// The input waiter's keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputKey {
    /// An owned, immutable or receiving object at an exact version.
    /// Available once the object store holds it.
    ObjectVersion { id: ObjectId, version: u64 },
    /// A package, which a transaction names without a version (as the
    /// reference's `InputKey::Package`). Available once the object store
    /// holds it live; `Executor` notifies it for each package it publishes.
    Package(ObjectId),
    /// Generation `generation` of `stream` in commit round `round` (see
    /// `consensus::sequence::SharedOrdinal`). Available once the generation
    /// table holds it.
    Generation {
        stream: StreamId,
        round: u32,
        generation: u32,
    },
    /// The stream as the object store holds it: a generation 0 the resolver
    /// has no earlier generation for. Always available.
    StreamLive(StreamId),
    /// Every funds reservation of the transaction has resolved. Available
    /// once the reservation table holds how; `FundsReserver` notifies it.
    Reserved(TxDigest),
}

/// The effects waiter's key: a transaction's effects, executed. Available
/// once the store holds its executed effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectsKey(pub TxDigest);

/// An accumulator version, counted in commits: the state once every chunk
/// of every commit before commit `.0` has settled. Commit `v`'s
/// transactions reserve funds against `v`, and settle into `v + 1`
/// (`docs/consensus_pipeline.md`, "Funds reservation").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccumulatorVersion(pub u64);

/// What a transaction waits on in `FundsReserver`, once per account it
/// withdraws from: `amount` at most, from `account`, against `version`. Not
/// unique: reservations are kept in arrival order, which is consensus order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub account: ObjectId,
    pub amount: u64,
    pub version: AccumulatorVersion,
}

/// The checkpoint waiter's key: the chunk at `height` settled. Available
/// once the chunk table holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkSettled(pub u64);

/// The sequence waiter's key: checkpoint `seq` built. Available once the
/// checkpoint store holds its summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CheckpointBuilt(pub u64);

/// The summary waiter's key: this validator's summary of checkpoint `seq`.
/// Available once the checkpoint store holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalSummary(pub u64);
