// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::arena::{Alloc, Ref};
use crate::base::{
    Digest, EffectsAuxDataDigest, ObjectDigest, ObjectId, ObjectKey, ObjectRef, SequenceNumber,
    SuiAddress, TransactionDigest, TransactionEffectsDigest, TransactionEventsDigest, U64Le,
};
use crate::error::{ParseError, Result};
use crate::execution_status::ExecutionStatus;
use crate::object::Owner;
use crate::reader::{Reader, WireRecord};
use crate::type_tag::{StructTag, TypeTag};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GasCostSummary {
    pub computation_cost: u64,
    pub storage_cost: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
}

impl GasCostSummary {
    pub fn parse(r: &mut Reader<'_>) -> Result<GasCostSummary> {
        Ok(GasCostSummary {
            computation_cost: r.u64()?,
            storage_cost: r.u64()?,
            storage_rebate: r.u64()?,
            non_refundable_storage_fee: r.u64()?,
        })
    }
}

fn parse_option_digest<'a>(r: &mut Reader<'a>) -> Result<Option<&'a Digest>> {
    Ok(if r.option()? {
        Some(Digest::parse(r)?)
    } else {
        None
    })
}

fn parse_digests<'a>(r: &mut Reader<'a>) -> Result<&'a [Digest]> {
    let digests: &[Digest] = r.record_vec()?;
    for d in digests {
        d.check()?;
    }
    Ok(digests)
}

/// `Vec<(ObjectRef, Owner)>`.
fn parse_owned_refs<'a, A: Alloc<'a>>(
    r: &mut Reader<'a>,
    a: &mut A,
) -> Result<&'a [(&'a ObjectRef, Owner<'a>)]> {
    let n = r.seq_len(size_of::<ObjectRef>() + Owner::MIN_WIRE_SIZE)?;
    let mut out = a.slice(n)?;
    for _ in 0..n {
        out.push((ObjectRef::parse(r)?, Owner::parse(r, a)?));
    }
    Ok(out.finish())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionEffectsV1<'a> {
    pub status: ExecutionStatus<'a>,
    pub executed_epoch: u64,
    pub gas_used: GasCostSummary,
    pub modified_at_versions: &'a [ObjectKey],
    pub shared_objects: &'a [ObjectRef],
    pub transaction_digest: &'a TransactionDigest,
    pub created: &'a [(&'a ObjectRef, Owner<'a>)],
    pub mutated: &'a [(&'a ObjectRef, Owner<'a>)],
    pub unwrapped: &'a [(&'a ObjectRef, Owner<'a>)],
    pub deleted: &'a [ObjectRef],
    pub unwrapped_then_deleted: &'a [ObjectRef],
    pub wrapped: &'a [ObjectRef],
    pub gas_object: (&'a ObjectRef, Owner<'a>),
    pub events_digest: Option<&'a TransactionEventsDigest>,
    pub dependencies: &'a [TransactionDigest],
}

impl<'a> TransactionEffectsV1<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionEffectsV1<'a>> {
        r.enter()?;
        let effects = TransactionEffectsV1 {
            status: ExecutionStatus::parse(r)?,
            executed_epoch: r.u64()?,
            gas_used: GasCostSummary::parse(r)?,
            modified_at_versions: r.record_vec()?,
            shared_objects: ObjectRef::parse_vec(r)?,
            transaction_digest: TransactionDigest::parse(r)?,
            created: parse_owned_refs(r, a)?,
            mutated: parse_owned_refs(r, a)?,
            unwrapped: parse_owned_refs(r, a)?,
            deleted: ObjectRef::parse_vec(r)?,
            unwrapped_then_deleted: ObjectRef::parse_vec(r)?,
            wrapped: ObjectRef::parse_vec(r)?,
            gas_object: (ObjectRef::parse(r)?, Owner::parse(r, a)?),
            events_digest: parse_option_digest(r)?,
            dependencies: parse_digests(r)?,
        };
        r.leave();
        Ok(effects)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectIn<'a> {
    NotExist,
    Exist {
        version: SequenceNumber,
        digest: &'a ObjectDigest,
        owner: Owner<'a>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccumulatorOperation {
    Merge,
    Split,
}

/// One `AccumulatorValue::EventDigest` entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct EventCommitment {
    pub index: U64Le,
    pub digest: Digest,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for EventCommitment {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccumulatorValue<'a> {
    Integer(u64),
    IntegerTuple(u64, u64),
    /// Not checked to be non-empty here.
    EventDigest(&'a [EventCommitment]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AccumulatorWriteV1<'a> {
    pub address: &'a SuiAddress,
    pub ty: TypeTag<'a>,
    pub operation: AccumulatorOperation,
    pub value: AccumulatorValue<'a>,
}

impl<'a> AccumulatorWriteV1<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<AccumulatorWriteV1<'a>> {
        r.enter()?;

        // `AccumulatorAddress`.
        r.enter()?;
        let address = SuiAddress::parse(r)?;
        let ty = TypeTag::parse(r, a)?;
        r.leave();

        let operation = match r.variant()? {
            0 => AccumulatorOperation::Merge,
            1 => AccumulatorOperation::Split,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "AccumulatorOperation",
                    tag,
                });
            }
        };

        let value = match r.variant()? {
            0 => AccumulatorValue::Integer(r.u64()?),
            1 => AccumulatorValue::IntegerTuple(r.u64()?, r.u64()?),
            2 => {
                let commitments: &[EventCommitment] = r.record_vec()?;
                for c in commitments {
                    c.digest.check()?;
                }
                AccumulatorValue::EventDigest(commitments)
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "AccumulatorValue",
                    tag,
                });
            }
        };

        r.leave();
        Ok(AccumulatorWriteV1 {
            address,
            ty,
            operation,
            value,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectOut<'a> {
    NotExist,
    ObjectWrite(&'a ObjectDigest, Owner<'a>),
    PackageWrite(SequenceNumber, &'a ObjectDigest),
    AccumulatorWriteV1(Ref<'a, AccumulatorWriteV1<'a>>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdOperation {
    None,
    Created,
    Deleted,
}

/// What happened to an object, as the reference's `created()`, `mutated()`
/// and the rest each work out from the three fields of a change. A change
/// is in at most one class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Created,
    Mutated,
    Unwrapped,
    Deleted,
    UnwrappedThenDeleted,
    Wrapped,
    AccumulatorWrite,
    /// Created and then wrapped or deleted by the same transaction. The
    /// reference lists these under none of its accessors.
    Transient,
    /// A combination the reference puts in no class, such as a package
    /// written to an id that neither existed nor was created.
    Unclassified,
}

impl ChangeKind {
    fn of(input: &ObjectIn<'_>, output: &ObjectOut<'_>, id_operation: IdOperation) -> ChangeKind {
        let existed = matches!(input, ObjectIn::Exist { .. });
        match (existed, output, id_operation) {
            (_, ObjectOut::AccumulatorWriteV1(_), _) => ChangeKind::AccumulatorWrite,
            (true, ObjectOut::ObjectWrite(..) | ObjectOut::PackageWrite(..), _) => {
                ChangeKind::Mutated
            }
            (
                false,
                ObjectOut::ObjectWrite(..) | ObjectOut::PackageWrite(..),
                IdOperation::Created,
            ) => ChangeKind::Created,
            (false, ObjectOut::ObjectWrite(..), IdOperation::None) => ChangeKind::Unwrapped,
            (true, ObjectOut::NotExist, IdOperation::Deleted) => ChangeKind::Deleted,
            (false, ObjectOut::NotExist, IdOperation::Deleted) => ChangeKind::UnwrappedThenDeleted,
            (true, ObjectOut::NotExist, IdOperation::None) => ChangeKind::Wrapped,
            (false, ObjectOut::ObjectWrite(..), IdOperation::Deleted)
            | (false, ObjectOut::PackageWrite(..), IdOperation::None | IdOperation::Deleted)
            | (true, ObjectOut::NotExist, IdOperation::Created)
            | (false, ObjectOut::NotExist, IdOperation::None) => ChangeKind::Unclassified,
            (false, ObjectOut::NotExist, IdOperation::Created) => ChangeKind::Transient,
        }
    }
}

/// A `changed_objects` entry: the id, then `EffectsObjectChange`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectChange<'a> {
    pub id: &'a ObjectId,
    pub input_state: ObjectIn<'a>,
    pub output_state: ObjectOut<'a>,
    pub id_operation: IdOperation,
    /// Derived from the three fields above while parsing.
    pub kind: ChangeKind,
}

impl<'a> ObjectChange<'a> {
    /// An id and three unit variants.
    pub const MIN_WIRE_SIZE: usize = 32 + 3;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<ObjectChange<'a>> {
        let id = ObjectId::parse(r)?;
        r.enter()?;

        let input_state = match r.variant()? {
            0 => ObjectIn::NotExist,
            1 => ObjectIn::Exist {
                version: r.u64()?,
                digest: ObjectDigest::parse(r)?,
                owner: Owner::parse(r, a)?,
            },
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ObjectIn",
                    tag,
                });
            }
        };

        r.enter()?;
        let output_state = match r.variant()? {
            0 => ObjectOut::NotExist,
            1 => ObjectOut::ObjectWrite(ObjectDigest::parse(r)?, Owner::parse(r, a)?),
            2 => ObjectOut::PackageWrite(r.u64()?, ObjectDigest::parse(r)?),
            3 => {
                let write = AccumulatorWriteV1::parse(r, a)?;
                ObjectOut::AccumulatorWriteV1(a.value(write)?)
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ObjectOut",
                    tag,
                });
            }
        };
        r.leave();

        let id_operation = match r.variant()? {
            0 => IdOperation::None,
            1 => IdOperation::Created,
            2 => IdOperation::Deleted,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "IDOperation",
                    tag,
                });
            }
        };

        r.leave();
        Ok(ObjectChange {
            id,
            input_state,
            output_state,
            id_operation,
            kind: ChangeKind::of(&input_state, &output_state, id_operation),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnchangedConsensusKind<'a> {
    ReadOnlyRoot(SequenceNumber, &'a ObjectDigest),
    MutateConsensusStreamEnded(SequenceNumber),
    ReadConsensusStreamEnded(SequenceNumber),
    Cancelled(SequenceNumber),
    PerEpochConfig,
}

impl<'a> UnchangedConsensusKind<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<UnchangedConsensusKind<'a>> {
        use UnchangedConsensusKind as K;
        Ok(match r.variant()? {
            0 => K::ReadOnlyRoot(r.u64()?, ObjectDigest::parse(r)?),
            1 => K::MutateConsensusStreamEnded(r.u64()?),
            2 => K::ReadConsensusStreamEnded(r.u64()?),
            3 => K::Cancelled(r.u64()?),
            4 => K::PerEpochConfig,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "UnchangedConsensusKind",
                    tag,
                });
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionEffectsV2<'a> {
    pub status: ExecutionStatus<'a>,
    pub executed_epoch: u64,
    pub gas_used: GasCostSummary,
    pub transaction_digest: &'a TransactionDigest,
    /// An index into `changed_objects`, not checked to be in bounds here.
    pub gas_object_index: Option<u32>,
    pub events_digest: Option<&'a TransactionEventsDigest>,
    pub dependencies: &'a [TransactionDigest],
    pub lamport_version: SequenceNumber,
    pub changed_objects: &'a [ObjectChange<'a>],
    pub unchanged_consensus_objects: &'a [(&'a ObjectId, UnchangedConsensusKind<'a>)],
    pub aux_data_digest: Option<&'a EffectsAuxDataDigest>,
}

impl<'a> TransactionEffectsV2<'a> {
    /// The changes of one class, in stored order.
    pub fn changes(&self, kind: ChangeKind) -> impl Iterator<Item = &'a ObjectChange<'a>> {
        self.changed_objects.iter().filter(move |c| c.kind == kind)
    }

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionEffectsV2<'a>> {
        r.enter()?;
        let status = ExecutionStatus::parse(r)?;
        let executed_epoch = r.u64()?;
        let gas_used = GasCostSummary::parse(r)?;
        let transaction_digest = TransactionDigest::parse(r)?;
        let gas_object_index = if r.option()? { Some(r.u32()?) } else { None };
        let events_digest = parse_option_digest(r)?;
        let dependencies = parse_digests(r)?;
        let lamport_version = r.u64()?;

        let n = r.seq_len(ObjectChange::MIN_WIRE_SIZE)?;
        let mut changed_objects = a.slice(n)?;
        for _ in 0..n {
            changed_objects.push(ObjectChange::parse(r, a)?);
        }
        let changed_objects = changed_objects.finish();

        let n = r.seq_len(32 + 1)?;
        let mut unchanged_consensus_objects = a.slice(n)?;
        for _ in 0..n {
            unchanged_consensus_objects
                .push((ObjectId::parse(r)?, UnchangedConsensusKind::parse(r)?));
        }
        let unchanged_consensus_objects = unchanged_consensus_objects.finish();

        let aux_data_digest = parse_option_digest(r)?;
        r.leave();
        Ok(TransactionEffectsV2 {
            status,
            executed_epoch,
            gas_used,
            transaction_digest,
            gas_object_index,
            events_digest,
            dependencies,
            lamport_version,
            changed_objects,
            unchanged_consensus_objects,
            aux_data_digest,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VersionedEffects<'a> {
    V1(Ref<'a, TransactionEffectsV1<'a>>),
    V2(TransactionEffectsV2<'a>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionEffects<'a> {
    /// The exact encoding, which is what gets hashed.
    pub bytes: &'a [u8],
    /// Computed once, from `bytes`, while parsing.
    pub digest: TransactionEffectsDigest,
    pub version: VersionedEffects<'a>,
}

impl<'a> TransactionEffects<'a> {
    /// A successful V2 with nothing optional and every sequence empty.
    pub const MIN_WIRE_SIZE: usize = 1 + 1 + 8 + 32 + 33 + 1 + 1 + 1 + 8 + 1 + 1 + 1;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionEffects<'a>> {
        let start = r.pos();
        r.enter()?;
        let version = match r.variant()? {
            0 => {
                let v1 = TransactionEffectsV1::parse(r, a)?;
                VersionedEffects::V1(a.value(v1)?)
            }
            1 => VersionedEffects::V2(TransactionEffectsV2::parse(r, a)?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "TransactionEffects",
                    tag,
                });
            }
        };
        r.leave();
        let bytes = r.span(start);
        Ok(TransactionEffects {
            bytes,
            digest: if A::BUILD {
                Digest::of("TransactionEffects", bytes)
            } else {
                Digest::ZERO
            },
            version,
        })
    }
}

/// The module name is not checked against the Move identifier grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event<'a> {
    pub package_id: &'a ObjectId,
    pub transaction_module: &'a str,
    pub sender: &'a SuiAddress,
    pub type_: StructTag<'a>,
    pub contents: &'a [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionEvents<'a> {
    /// The exact encoding, which is what gets hashed.
    pub bytes: &'a [u8],
    pub data: &'a [Event<'a>],
}

impl<'a> TransactionEvents<'a> {
    /// Hashed on demand: the reference does not treat events as a message.
    pub fn digest(&self) -> TransactionEventsDigest {
        Digest::of("TransactionEvents", self.bytes)
    }

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionEvents<'a>> {
        let start = r.pos();
        r.enter()?;
        // Two ids, a struct tag of an address and three lengths, and two more lengths.
        let n = r.seq_len(32 + 1 + 32 + (32 + 3) + 1)?;
        let mut data = a.slice(n)?;
        for _ in 0..n {
            r.enter()?;
            data.push(Event {
                package_id: ObjectId::parse(r)?,
                transaction_module: r.str()?,
                sender: SuiAddress::parse(r)?,
                type_: StructTag::parse(r, a)?,
                contents: r.byte_vec()?,
            });
            r.leave();
        }
        r.leave();
        Ok(TransactionEvents {
            bytes: r.span(start),
            data: data.finish(),
        })
    }
}

// Mainnet p99 of arena over wire size: 0.61 and 0.88.
crate::impl_wire!(TransactionEffects, guess = 10);
crate::impl_wire!(TransactionEvents, guess = 15);
