// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Built, Bump, Writer};
use crate::base::{Digest, ObjectId};
use crate::effects::{
    AccumulatorOperation, AccumulatorValue, GasCostSummary, IdOperation, ObjectIn, ObjectOut,
    UnchangedConsensusKind,
};
use crate::execution_status::ExecutionStatus;

/// One `changed_objects` entry before it is written.
struct Change<'a> {
    id: ObjectId,
    input: ObjectIn<'a>,
    output: ObjectOut<'a>,
    operation: IdOperation,
}

/// Builds `TransactionEffects::V2`.
///
/// Changed objects must be given in increasing id order, which is how the
/// reference emits them; dependencies may come in any order and are sorted
/// and deduplicated at `finish`, as the reference's `BTreeSet` does.
pub struct EffectsBuilder<'a> {
    bump: &'a Bump,
    status: ExecutionStatus<'a>,
    executed_epoch: u64,
    gas_used: GasCostSummary,
    transaction_digest: Digest,
    lamport_version: u64,
    events_digest: Option<Digest>,
    aux_data_digest: Option<Digest>,
    dependencies: containers::Vec<'a, Digest>,
    changed: containers::Vec<'a, Change<'a>>,
    gas_object_index: Option<u32>,
    unchanged: containers::Vec<'a, (ObjectId, UnchangedConsensusKind<'a>)>,
    /// A running estimate of the encoded size.
    bytes: usize,
}

impl<'a> EffectsBuilder<'a> {
    pub fn new_in(
        bump: &'a Bump,
        status: ExecutionStatus<'a>,
        executed_epoch: u64,
        gas_used: GasCostSummary,
        transaction_digest: Digest,
        lamport_version: u64,
    ) -> EffectsBuilder<'a> {
        EffectsBuilder {
            bump,
            status,
            executed_epoch,
            gas_used,
            transaction_digest,
            lamport_version,
            events_digest: None,
            aux_data_digest: None,
            dependencies: containers::Vec::with_capacity_in(8, bump),
            changed: containers::Vec::with_capacity_in(8, bump),
            gas_object_index: None,
            unchanged: containers::Vec::with_capacity_in(4, bump),
            bytes: 128,
        }
    }

    pub fn events_digest(&mut self, digest: Option<Digest>) -> &mut Self {
        self.events_digest = digest;
        self
    }

    pub fn aux_data_digest(&mut self, digest: Option<Digest>) -> &mut Self {
        self.aux_data_digest = digest;
        self
    }

    pub fn dependency(&mut self, digest: Digest) -> &mut Self {
        self.dependencies.push(digest);
        self.bytes += 33;
        self
    }

    /// The next changed object; `id` must exceed the previous one's.
    pub fn change(
        &mut self,
        id: ObjectId,
        input: ObjectIn<'a>,
        output: ObjectOut<'a>,
        operation: IdOperation,
    ) -> &mut Self {
        debug_assert!(self.changed.last().is_none_or(|c| c.id < id));
        self.bytes += 32 + 1 + 42 + 1 + 34 + 1;
        self.changed.push(Change {
            id,
            input,
            output,
            operation,
        });
        self
    }

    /// Marks the change pushed last as the gas object.
    pub fn gas_object_is_last(&mut self) -> &mut Self {
        self.gas_object_index = Some(self.changed.len() as u32 - 1);
        self
    }

    pub fn unchanged(&mut self, id: ObjectId, kind: UnchangedConsensusKind<'a>) -> &mut Self {
        self.unchanged.push((id, kind));
        self.bytes += 32 + 1 + 8 + 33;
        self
    }

    pub fn finish(mut self) -> Built<'a> {
        self.dependencies.sort_unstable();
        self.dependencies.dedup();

        let mut w = Writer::new_in(self.bump, self.bytes);
        w.u8(1);
        w.execution_status(&self.status);
        w.u64(self.executed_epoch);
        w.u64(self.gas_used.computation_cost);
        w.u64(self.gas_used.storage_cost);
        w.u64(self.gas_used.storage_rebate);
        w.u64(self.gas_used.non_refundable_storage_fee);
        w.digest(&self.transaction_digest);
        match self.gas_object_index {
            Some(i) => {
                w.u8(1);
                w.u32(i);
            }
            None => w.u8(0),
        }
        w.option_digest(self.events_digest.as_ref());
        w.len_prefix(self.dependencies.len());
        for d in &self.dependencies {
            w.digest(d);
        }
        w.u64(self.lamport_version);
        w.len_prefix(self.changed.len());
        for c in &self.changed {
            w.raw(&c.id.0);
            object_in(&mut w, &c.input);
            object_out(&mut w, &c.output);
            w.u8(match c.operation {
                IdOperation::None => 0,
                IdOperation::Created => 1,
                IdOperation::Deleted => 2,
            });
        }
        w.len_prefix(self.unchanged.len());
        for (id, kind) in &self.unchanged {
            w.raw(&id.0);
            unchanged_kind(&mut w, kind);
        }
        w.option_digest(self.aux_data_digest.as_ref());
        w.finish("TransactionEffects")
    }
}

fn object_in(w: &mut Writer<'_>, input: &ObjectIn<'_>) {
    match input {
        ObjectIn::NotExist => w.u8(0),
        ObjectIn::Exist {
            version,
            digest,
            owner,
        } => {
            w.u8(1);
            w.u64(*version);
            w.digest(digest);
            w.owner(owner);
        }
    }
}

fn object_out(w: &mut Writer<'_>, output: &ObjectOut<'_>) {
    match output {
        ObjectOut::NotExist => w.u8(0),
        ObjectOut::ObjectWrite(digest, owner) => {
            w.u8(1);
            w.digest(digest);
            w.owner(owner);
        }
        ObjectOut::PackageWrite(version, digest) => {
            w.u8(2);
            w.u64(*version);
            w.digest(digest);
        }
        ObjectOut::AccumulatorWriteV1(write) => {
            w.u8(3);
            w.address(write.address);
            w.type_tag(&write.ty);
            w.u8(match write.operation {
                AccumulatorOperation::Merge => 0,
                AccumulatorOperation::Split => 1,
            });
            match write.value {
                AccumulatorValue::Integer(v) => {
                    w.u8(0);
                    w.u64(v);
                }
                AccumulatorValue::IntegerTuple(a, b) => {
                    w.u8(1);
                    w.u64(a);
                    w.u64(b);
                }
                AccumulatorValue::EventDigest(commitments) => {
                    w.u8(2);
                    w.len_prefix(commitments.len());
                    for c in commitments {
                        w.raw(&c.index.0);
                        w.digest(&c.digest);
                    }
                }
            }
        }
    }
}

fn unchanged_kind(w: &mut Writer<'_>, kind: &UnchangedConsensusKind<'_>) {
    use UnchangedConsensusKind as K;
    match kind {
        K::ReadOnlyRoot(version, digest) => {
            w.u8(0);
            w.u64(*version);
            w.digest(digest);
        }
        K::MutateConsensusStreamEnded(v) => {
            w.u8(1);
            w.u64(*v);
        }
        K::ReadConsensusStreamEnded(v) => {
            w.u8(2);
            w.u64(*v);
        }
        K::Cancelled(v) => {
            w.u8(3);
            w.u64(*v);
        }
        K::PerEpochConfig => w.u8(4),
    }
}
