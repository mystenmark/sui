// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The keys work waits on, one type per waiter. A key is notified only once
//! the state its availability check reads holds it.

use messages::base::{Digest, Hashed, ObjectId};

/// A transaction digest, as this validator computed it.
pub type TxDigest = Hashed;

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
/// once the store holds its executed effects. A plain digest, hashed whole:
/// clients wait on digests they name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectsKey(pub Digest);

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

/// The built waiter's key: this validator built checkpoint `seq`. Available
/// once the checkpoint store holds it, or committed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CheckpointBuilt(pub u64);

/// A chunk transaction's place in its epoch's causal order: its chunk's
/// height, and its position among the chunk's transactions (settlements and
/// the barrier have none). What a transaction can block on in execution is
/// produced at a lower index, or by a settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CausalIndex {
    pub height: u64,
    pub position: u32,
}

/// The gate waiter's keys: what a chunk waits for before the randomness
/// gate passes it on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GateKey {
    /// The synced scheduler is not running the chunk at `height`. Available
    /// once the chunk lock does not hold it.
    ChunkFree(u64),
    /// Randomness round `round`'s random bytes, its value. Available once
    /// the epoch store records them.
    RoundBytes(u64),
}

/// The output waiter's keys: what consensus output waits for before it is
/// written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckpointProgress {
    /// The consensus path built checkpoint `seq`, and every one before it,
    /// in this run: every transaction of their chunks executed or was
    /// checked. Available once the checkpoint store says so.
    BuiltHere(u64),
    /// Checkpoint `seq` committed. Available once the checkpoint store says
    /// so.
    Committed(u64),
}
