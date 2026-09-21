// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::base::{ObjectId, SequenceNumber, SuiAddress, TransactionDigest};
use super::type_tag::{StructTag, TypeTag};
use crate::object as view;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RawPartySerde {
    pub default_permissions: u64,
    pub members: Vec<(SuiAddress, u64)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    AddressOwner(SuiAddress),
    ObjectOwner(SuiAddress),
    Shared {
        initial_shared_version: SequenceNumber,
    },
    Immutable,
    ConsensusAddressOwner {
        start_version: SequenceNumber,
        owner: SuiAddress,
    },
    Party {
        start_version: SequenceNumber,
        permissions: RawPartySerde,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename = "MoveObjectType_")]
pub enum MoveObjectTypeInner {
    Other(StructTag),
    GasCoin,
    StakedSui,
    Coin(TypeTag),
    SuiBalanceAccumulatorField,
    BalanceAccumulatorField(TypeTag),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MoveObjectType(pub MoveObjectTypeInner);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MoveObject {
    pub type_: MoveObjectType,
    pub has_public_transfer: bool,
    pub version: SequenceNumber,
    #[serde(with = "serde_bytes")]
    pub contents: Vec<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TypeOrigin {
    pub module_name: String,
    pub datatype_name: String,
    pub package: ObjectId,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UpgradeInfo {
    pub upgraded_id: ObjectId,
    pub upgraded_version: SequenceNumber,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MovePackage {
    pub id: ObjectId,
    pub version: SequenceNumber,
    #[serde(with = "super::bytes_map")]
    pub module_map: BTreeMap<String, Vec<u8>>,
    pub type_origin_table: Vec<TypeOrigin>,
    pub linkage_table: BTreeMap<ObjectId, UpgradeInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Data {
    Move(MoveObject),
    Package(MovePackage),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub data: Data,
    pub owner: Owner,
    pub previous_transaction: TransactionDigest,
    pub storage_rebate: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum GenesisObject {
    RawObject { data: Data, owner: Owner },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectInfoRequestKind {
    LatestObjectInfo,
    PastObjectInfoDebug(SequenceNumber),
}

impl From<&view::PartyMember> for (SuiAddress, u64) {
    fn from(v: &view::PartyMember) -> Self {
        (SuiAddress::from(&v.address), v.permissions.get())
    }
}

impl From<&view::Owner<'_>> for Owner {
    fn from(v: &view::Owner<'_>) -> Self {
        match v {
            view::Owner::AddressOwner(address) => Owner::AddressOwner(SuiAddress::from(*address)),
            view::Owner::ObjectOwner(address) => Owner::ObjectOwner(SuiAddress::from(*address)),
            view::Owner::Shared {
                initial_shared_version,
            } => Owner::Shared {
                initial_shared_version: SequenceNumber(*initial_shared_version),
            },
            view::Owner::Immutable => Owner::Immutable,
            view::Owner::ConsensusAddressOwner {
                start_version,
                owner,
            } => Owner::ConsensusAddressOwner {
                start_version: SequenceNumber(*start_version),
                owner: SuiAddress::from(*owner),
            },
            view::Owner::Party(party) => Owner::Party {
                start_version: SequenceNumber(party.start_version),
                permissions: RawPartySerde {
                    default_permissions: party.default_permissions,
                    members: party.members.iter().map(Into::into).collect(),
                },
            },
        }
    }
}

impl From<&view::MoveObjectType<'_>> for MoveObjectType {
    fn from(v: &view::MoveObjectType<'_>) -> Self {
        MoveObjectType(match v {
            view::MoveObjectType::Other(tag) => MoveObjectTypeInner::Other(StructTag::from(tag)),
            view::MoveObjectType::GasCoin => MoveObjectTypeInner::GasCoin,
            view::MoveObjectType::StakedSui => MoveObjectTypeInner::StakedSui,
            view::MoveObjectType::Coin(tag) => MoveObjectTypeInner::Coin(TypeTag::from(tag)),
            view::MoveObjectType::SuiBalanceAccumulatorField => {
                MoveObjectTypeInner::SuiBalanceAccumulatorField
            }
            view::MoveObjectType::BalanceAccumulatorField(tag) => {
                MoveObjectTypeInner::BalanceAccumulatorField(TypeTag::from(tag))
            }
        })
    }
}

impl From<&view::MoveObject<'_>> for MoveObject {
    fn from(v: &view::MoveObject<'_>) -> Self {
        MoveObject {
            type_: MoveObjectType::from(&v.type_),
            has_public_transfer: v.has_public_transfer,
            version: SequenceNumber(v.version),
            contents: v.contents.to_vec(),
        }
    }
}

impl From<&view::TypeOrigin<'_>> for TypeOrigin {
    fn from(v: &view::TypeOrigin<'_>) -> Self {
        TypeOrigin {
            module_name: v.module_name.to_owned(),
            datatype_name: v.datatype_name.to_owned(),
            package: ObjectId::from(v.package),
        }
    }
}

impl From<&view::Linkage> for (ObjectId, UpgradeInfo) {
    fn from(v: &view::Linkage) -> Self {
        (
            ObjectId::from(&v.original_id),
            UpgradeInfo {
                upgraded_id: ObjectId::from(&v.upgraded_id),
                upgraded_version: SequenceNumber(v.upgraded_version.get()),
            },
        )
    }
}

impl From<&view::MovePackage<'_>> for MovePackage {
    fn from(v: &view::MovePackage<'_>) -> Self {
        MovePackage {
            id: ObjectId::from(v.id),
            version: SequenceNumber(v.version),
            module_map: v
                .module_map
                .iter()
                .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
                .collect(),
            type_origin_table: v.type_origin_table.iter().map(TypeOrigin::from).collect(),
            linkage_table: v.linkage_table.iter().map(Into::into).collect(),
        }
    }
}

impl From<&view::Data<'_>> for Data {
    fn from(v: &view::Data<'_>) -> Self {
        match v {
            view::Data::Move(object) => Data::Move(MoveObject::from(object)),
            view::Data::Package(package) => Data::Package(MovePackage::from(package)),
        }
    }
}

impl From<&view::Object<'_>> for Object {
    fn from(v: &view::Object<'_>) -> Self {
        Object {
            data: Data::from(&v.data),
            owner: Owner::from(&v.owner),
            previous_transaction: TransactionDigest::from(v.previous_transaction),
            storage_rebate: v.storage_rebate,
        }
    }
}

impl From<&view::GenesisObject<'_>> for GenesisObject {
    fn from(v: &view::GenesisObject<'_>) -> Self {
        GenesisObject::RawObject {
            data: Data::from(&v.data),
            owner: Owner::from(&v.owner),
        }
    }
}
