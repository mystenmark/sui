// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{
    Digest, EffectsAuxDataDigest, ObjectDigest, ObjectId, ObjectRef, SequenceNumber, SuiAddress,
    TransactionDigest, TransactionEventsDigest,
};
use super::execution_status::ExecutionStatus;
use super::object::Owner;
use super::type_tag::{StructTag, TypeTag};
use crate::effects as view;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GasCostSummary {
    #[serde(rename = "computationCost")]
    pub computation_cost: u64,
    #[serde(rename = "storageCost")]
    pub storage_cost: u64,
    #[serde(rename = "storageRebate")]
    pub storage_rebate: u64,
    #[serde(rename = "nonRefundableStorageFee")]
    pub non_refundable_storage_fee: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionEffectsV1 {
    pub status: ExecutionStatus,
    pub executed_epoch: u64,
    pub gas_used: GasCostSummary,
    pub modified_at_versions: Vec<(ObjectId, SequenceNumber)>,
    pub shared_objects: Vec<ObjectRef>,
    pub transaction_digest: TransactionDigest,
    pub created: Vec<(ObjectRef, Owner)>,
    pub mutated: Vec<(ObjectRef, Owner)>,
    pub unwrapped: Vec<(ObjectRef, Owner)>,
    pub deleted: Vec<ObjectRef>,
    pub unwrapped_then_deleted: Vec<ObjectRef>,
    pub wrapped: Vec<ObjectRef>,
    pub gas_object: (ObjectRef, Owner),
    pub events_digest: Option<TransactionEventsDigest>,
    pub dependencies: Vec<TransactionDigest>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ObjectIn {
    NotExist,
    Exist(((SequenceNumber, ObjectDigest), Owner)),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AccumulatorAddress {
    pub address: SuiAddress,
    pub ty: TypeTag,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccumulatorOperation {
    Merge,
    Split,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AccumulatorValue {
    Integer(u64),
    IntegerTuple(u64, u64),
    EventDigest(Vec<(u64, Digest)>),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AccumulatorWriteV1 {
    pub address: AccumulatorAddress,
    pub operation: AccumulatorOperation,
    pub value: AccumulatorValue,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ObjectOut {
    NotExist,
    ObjectWrite((ObjectDigest, Owner)),
    PackageWrite((SequenceNumber, ObjectDigest)),
    AccumulatorWriteV1(AccumulatorWriteV1),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename = "IDOperation")]
pub enum IdOperation {
    None,
    Created,
    Deleted,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EffectsObjectChange {
    pub input_state: ObjectIn,
    pub output_state: ObjectOut,
    pub id_operation: IdOperation,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnchangedConsensusKind {
    ReadOnlyRoot((SequenceNumber, ObjectDigest)),
    MutateConsensusStreamEnded(SequenceNumber),
    ReadConsensusStreamEnded(SequenceNumber),
    Cancelled(SequenceNumber),
    PerEpochConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionEffectsV2 {
    pub status: ExecutionStatus,
    pub executed_epoch: u64,
    pub gas_used: GasCostSummary,
    pub transaction_digest: TransactionDigest,
    pub gas_object_index: Option<u32>,
    pub events_digest: Option<TransactionEventsDigest>,
    pub dependencies: Vec<TransactionDigest>,
    pub lamport_version: SequenceNumber,
    pub changed_objects: Vec<(ObjectId, EffectsObjectChange)>,
    pub unchanged_consensus_objects: Vec<(ObjectId, UnchangedConsensusKind)>,
    pub aux_data_digest: Option<EffectsAuxDataDigest>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransactionEffects {
    V1(Box<TransactionEffectsV1>),
    V2(Box<TransactionEffectsV2>),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub package_id: ObjectId,
    pub transaction_module: String,
    pub sender: SuiAddress,
    pub type_: StructTag,
    #[serde(with = "serde_bytes")]
    pub contents: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionEvents {
    pub data: Vec<Event>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteKind {
    Normal,
    UnwrapThenDelete,
    Wrap,
}

impl From<&view::GasCostSummary> for GasCostSummary {
    fn from(v: &view::GasCostSummary) -> Self {
        GasCostSummary {
            computation_cost: v.computation_cost,
            storage_cost: v.storage_cost,
            storage_rebate: v.storage_rebate,
            non_refundable_storage_fee: v.non_refundable_storage_fee,
        }
    }
}

fn owned_ref(v: &(&crate::base::ObjectRef, crate::object::Owner<'_>)) -> (ObjectRef, Owner) {
    (ObjectRef::from(v.0), Owner::from(&v.1))
}

impl From<&view::TransactionEffectsV1<'_>> for TransactionEffectsV1 {
    fn from(v: &view::TransactionEffectsV1<'_>) -> Self {
        TransactionEffectsV1 {
            status: ExecutionStatus::from(&v.status),
            executed_epoch: v.executed_epoch,
            gas_used: GasCostSummary::from(&v.gas_used),
            modified_at_versions: v.modified_at_versions.iter().map(Into::into).collect(),
            shared_objects: v.shared_objects.iter().map(ObjectRef::from).collect(),
            transaction_digest: TransactionDigest::from(v.transaction_digest),
            created: v.created.iter().map(owned_ref).collect(),
            mutated: v.mutated.iter().map(owned_ref).collect(),
            unwrapped: v.unwrapped.iter().map(owned_ref).collect(),
            deleted: v.deleted.iter().map(ObjectRef::from).collect(),
            unwrapped_then_deleted: v
                .unwrapped_then_deleted
                .iter()
                .map(ObjectRef::from)
                .collect(),
            wrapped: v.wrapped.iter().map(ObjectRef::from).collect(),
            gas_object: owned_ref(&v.gas_object),
            events_digest: v.events_digest.map(TransactionEventsDigest::from),
            dependencies: v.dependencies.iter().map(TransactionDigest::from).collect(),
        }
    }
}

impl From<&view::ObjectIn<'_>> for ObjectIn {
    fn from(v: &view::ObjectIn<'_>) -> Self {
        match v {
            view::ObjectIn::NotExist => ObjectIn::NotExist,
            view::ObjectIn::Exist {
                version,
                digest,
                owner,
            } => ObjectIn::Exist((
                (SequenceNumber(*version), ObjectDigest::from(*digest)),
                Owner::from(owner),
            )),
        }
    }
}

impl From<&view::AccumulatorOperation> for AccumulatorOperation {
    fn from(v: &view::AccumulatorOperation) -> Self {
        match v {
            view::AccumulatorOperation::Merge => AccumulatorOperation::Merge,
            view::AccumulatorOperation::Split => AccumulatorOperation::Split,
        }
    }
}

impl From<&view::EventCommitment> for (u64, Digest) {
    fn from(v: &view::EventCommitment) -> Self {
        (v.index.get(), Digest::from(&v.digest))
    }
}

impl From<&view::AccumulatorValue<'_>> for AccumulatorValue {
    fn from(v: &view::AccumulatorValue<'_>) -> Self {
        match v {
            view::AccumulatorValue::Integer(n) => AccumulatorValue::Integer(*n),
            view::AccumulatorValue::IntegerTuple(a, b) => AccumulatorValue::IntegerTuple(*a, *b),
            view::AccumulatorValue::EventDigest(commitments) => {
                AccumulatorValue::EventDigest(commitments.iter().map(Into::into).collect())
            }
        }
    }
}

impl From<&view::AccumulatorWriteV1<'_>> for AccumulatorWriteV1 {
    fn from(v: &view::AccumulatorWriteV1<'_>) -> Self {
        AccumulatorWriteV1 {
            address: AccumulatorAddress {
                address: SuiAddress::from(v.address),
                ty: TypeTag::from(&v.ty),
            },
            operation: AccumulatorOperation::from(&v.operation),
            value: AccumulatorValue::from(&v.value),
        }
    }
}

impl From<&view::ObjectOut<'_>> for ObjectOut {
    fn from(v: &view::ObjectOut<'_>) -> Self {
        match v {
            view::ObjectOut::NotExist => ObjectOut::NotExist,
            view::ObjectOut::ObjectWrite(digest, owner) => {
                ObjectOut::ObjectWrite((ObjectDigest::from(*digest), Owner::from(owner)))
            }
            view::ObjectOut::PackageWrite(version, digest) => {
                ObjectOut::PackageWrite((SequenceNumber(*version), ObjectDigest::from(*digest)))
            }
            view::ObjectOut::AccumulatorWriteV1(write) => {
                ObjectOut::AccumulatorWriteV1(AccumulatorWriteV1::from(&**write))
            }
        }
    }
}

impl From<&view::IdOperation> for IdOperation {
    fn from(v: &view::IdOperation) -> Self {
        match v {
            view::IdOperation::None => IdOperation::None,
            view::IdOperation::Created => IdOperation::Created,
            view::IdOperation::Deleted => IdOperation::Deleted,
        }
    }
}

/// The view's `id` is the other half of the `changed_objects` entry.
impl From<&view::ObjectChange<'_>> for EffectsObjectChange {
    fn from(v: &view::ObjectChange<'_>) -> Self {
        EffectsObjectChange {
            input_state: ObjectIn::from(&v.input_state),
            output_state: ObjectOut::from(&v.output_state),
            id_operation: IdOperation::from(&v.id_operation),
        }
    }
}

impl From<&view::UnchangedConsensusKind<'_>> for UnchangedConsensusKind {
    fn from(v: &view::UnchangedConsensusKind<'_>) -> Self {
        use UnchangedConsensusKind as B;
        use view::UnchangedConsensusKind as V;
        match *v {
            V::ReadOnlyRoot(version, digest) => {
                B::ReadOnlyRoot((SequenceNumber(version), ObjectDigest::from(digest)))
            }
            V::MutateConsensusStreamEnded(version) => {
                B::MutateConsensusStreamEnded(SequenceNumber(version))
            }
            V::ReadConsensusStreamEnded(version) => {
                B::ReadConsensusStreamEnded(SequenceNumber(version))
            }
            V::Cancelled(version) => B::Cancelled(SequenceNumber(version)),
            V::PerEpochConfig => B::PerEpochConfig,
        }
    }
}

impl From<&view::TransactionEffectsV2<'_>> for TransactionEffectsV2 {
    fn from(v: &view::TransactionEffectsV2<'_>) -> Self {
        TransactionEffectsV2 {
            status: ExecutionStatus::from(&v.status),
            executed_epoch: v.executed_epoch,
            gas_used: GasCostSummary::from(&v.gas_used),
            transaction_digest: TransactionDigest::from(v.transaction_digest),
            gas_object_index: v.gas_object_index,
            events_digest: v.events_digest.map(TransactionEventsDigest::from),
            dependencies: v.dependencies.iter().map(TransactionDigest::from).collect(),
            lamport_version: SequenceNumber(v.lamport_version),
            changed_objects: v
                .changed_objects
                .iter()
                .map(|change| (ObjectId::from(change.id), EffectsObjectChange::from(change)))
                .collect(),
            unchanged_consensus_objects: v
                .unchanged_consensus_objects
                .iter()
                .map(|(id, kind)| (ObjectId::from(*id), UnchangedConsensusKind::from(kind)))
                .collect(),
            aux_data_digest: v.aux_data_digest.map(EffectsAuxDataDigest::from),
        }
    }
}

impl From<&view::TransactionEffects<'_>> for TransactionEffects {
    fn from(v: &view::TransactionEffects<'_>) -> Self {
        match &v.version {
            view::VersionedEffects::V1(v1) => {
                TransactionEffects::V1(Box::new(TransactionEffectsV1::from(&**v1)))
            }
            view::VersionedEffects::V2(v2) => {
                TransactionEffects::V2(Box::new(TransactionEffectsV2::from(v2)))
            }
        }
    }
}

impl From<&view::Event<'_>> for Event {
    fn from(v: &view::Event<'_>) -> Self {
        Event {
            package_id: ObjectId::from(v.package_id),
            transaction_module: v.transaction_module.to_owned(),
            sender: SuiAddress::from(v.sender),
            type_: StructTag::from(&v.type_),
            contents: v.contents.to_vec(),
        }
    }
}

impl From<&view::TransactionEvents<'_>> for TransactionEvents {
    fn from(v: &view::TransactionEvents<'_>) -> Self {
        TransactionEvents {
            data: v.data.iter().map(Event::from).collect(),
        }
    }
}
