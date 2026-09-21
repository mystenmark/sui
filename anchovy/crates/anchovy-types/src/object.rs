// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::arena::{Alloc, Ref};
use crate::base::{ObjectId, SequenceNumber, SuiAddress, TransactionDigest, U64Le};
use crate::error::{ParseError, Result};
use crate::reader::{Reader, WireRecord};
use crate::type_tag::{StructTag, TypeTag};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner<'a> {
    AddressOwner(&'a SuiAddress),
    ObjectOwner(&'a SuiAddress),
    Shared {
        initial_shared_version: SequenceNumber,
    },
    Immutable,
    ConsensusAddressOwner {
        start_version: SequenceNumber,
        owner: &'a SuiAddress,
    },
    Party(Ref<'a, Party<'a>>),
}

/// Permission bits, member order and duplicates are not checked here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Party<'a> {
    pub start_version: SequenceNumber,
    pub default_permissions: u64,
    pub members: &'a [PartyMember],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct PartyMember {
    pub address: SuiAddress,
    pub permissions: U64Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for PartyMember {}

impl<'a> Owner<'a> {
    pub const MIN_WIRE_SIZE: usize = 1;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<Owner<'a>> {
        r.enter()?;
        let owner = match r.variant()? {
            0 => Owner::AddressOwner(SuiAddress::parse(r)?),
            1 => Owner::ObjectOwner(SuiAddress::parse(r)?),
            2 => Owner::Shared {
                initial_shared_version: r.u64()?,
            },
            3 => Owner::Immutable,
            4 => Owner::ConsensusAddressOwner {
                start_version: r.u64()?,
                owner: SuiAddress::parse(r)?,
            },
            5 => {
                let start_version = r.u64()?;
                r.enter()?;
                let default_permissions = r.u64()?;
                let members = r.record_vec()?;
                r.leave();
                Owner::Party(a.value(Party {
                    start_version,
                    default_permissions,
                    members,
                })?)
            }
            tag => return Err(ParseError::UnknownVariant { ty: "Owner", tag }),
        };
        r.leave();
        Ok(owner)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveObjectType<'a> {
    Other(StructTag<'a>),
    GasCoin,
    StakedSui,
    Coin(TypeTag<'a>),
    SuiBalanceAccumulatorField,
    BalanceAccumulatorField(TypeTag<'a>),
}

impl<'a> MoveObjectType<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<MoveObjectType<'a>> {
        // A newtype struct around an enum.
        r.enter()?;
        r.enter()?;
        let ty = match r.variant()? {
            0 => MoveObjectType::Other(StructTag::parse(r, a)?),
            1 => MoveObjectType::GasCoin,
            2 => MoveObjectType::StakedSui,
            3 => MoveObjectType::Coin(TypeTag::parse(r, a)?),
            4 => MoveObjectType::SuiBalanceAccumulatorField,
            5 => MoveObjectType::BalanceAccumulatorField(TypeTag::parse(r, a)?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "MoveObjectType",
                    tag,
                });
            }
        };
        r.leave();
        r.leave();
        Ok(ty)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MoveObject<'a> {
    pub type_: MoveObjectType<'a>,
    pub has_public_transfer: bool,
    pub version: SequenceNumber,
    pub contents: &'a [u8],
}

impl<'a> MoveObject<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<MoveObject<'a>> {
        r.enter()?;
        let type_ = MoveObjectType::parse(r, a)?;
        let has_public_transfer = r.bool()?;
        let version = r.u64()?;
        let contents = r.byte_vec()?;
        r.leave();
        Ok(MoveObject {
            type_,
            has_public_transfer,
            version,
            contents,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeOrigin<'a> {
    pub module_name: &'a str,
    pub datatype_name: &'a str,
    pub package: &'a ObjectId,
}

/// One `linkage_table` entry: the key, then `UpgradeInfo`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Linkage {
    pub original_id: ObjectId,
    pub upgraded_id: ObjectId,
    pub upgraded_version: U64Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for Linkage {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MovePackage<'a> {
    pub id: &'a ObjectId,
    pub version: SequenceNumber,
    /// Module name to bytecode, in increasing order of serialized name.
    pub module_map: &'a [(&'a str, &'a [u8])],
    pub type_origin_table: &'a [TypeOrigin<'a>],
    /// In increasing order of `original_id`.
    pub linkage_table: &'a [Linkage],
}

impl<'a> MovePackage<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<MovePackage<'a>> {
        r.enter()?;
        let id = ObjectId::parse(r)?;
        let version = r.u64()?;

        // Each entry is at least two length bytes.
        let n = r.seq_len(2)?;
        let mut module_map = a.slice(n)?;
        let mut prev_key: &[u8] = &[];
        for _ in 0..n {
            let key_start = r.pos();
            let name = r.str()?;
            // `bcs` orders map keys by their serialized bytes, length prefix
            // included. No key is empty, so the initial `prev_key` passes.
            let key = r.span(key_start);
            if prev_key >= key {
                return Err(ParseError::NonCanonicalMap);
            }
            prev_key = key;
            module_map.push((name, r.byte_vec()?));
        }
        let module_map = module_map.finish();

        let n = r.seq_len(2 + ObjectId::LENGTH)?;
        let mut type_origin_table = a.slice(n)?;
        for _ in 0..n {
            r.enter()?;
            type_origin_table.push(TypeOrigin {
                module_name: r.str()?,
                datatype_name: r.str()?,
                package: ObjectId::parse(r)?,
            });
            r.leave();
        }
        let type_origin_table = type_origin_table.finish();

        let linkage_table: &[Linkage] = r.record_vec()?;
        for pair in linkage_table.windows(2) {
            if pair[0].original_id >= pair[1].original_id {
                return Err(ParseError::NonCanonicalMap);
            }
        }
        r.leave();
        Ok(MovePackage {
            id,
            version,
            module_map,
            type_origin_table,
            linkage_table,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Data<'a> {
    Move(MoveObject<'a>),
    Package(MovePackage<'a>),
}

impl<'a> Data<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<Data<'a>> {
        r.enter()?;
        let data = match r.variant()? {
            0 => Data::Move(MoveObject::parse(r, a)?),
            1 => Data::Package(MovePackage::parse(r, a)?),
            tag => return Err(ParseError::UnknownVariant { ty: "Data", tag }),
        };
        r.leave();
        Ok(data)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Object<'a> {
    /// The exact encoding, which is what gets hashed.
    pub bytes: &'a [u8],
    pub data: Data<'a>,
    pub owner: Owner<'a>,
    pub previous_transaction: &'a TransactionDigest,
    pub storage_rebate: u64,
}

impl<'a> Object<'a> {
    /// A `GasCoin` `MoveObject` with empty contents, an `Immutable` owner, a
    /// digest and a rebate.
    pub const MIN_WIRE_SIZE: usize = 1 + (1 + 1 + 8 + 1) + 1 + 33 + 8;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<Object<'a>> {
        let start = r.pos();
        r.enter()?;
        let data = Data::parse(r, a)?;
        let owner = Owner::parse(r, a)?;
        let previous_transaction = TransactionDigest::parse(r)?;
        let storage_rebate = r.u64()?;
        r.leave();
        Ok(Object {
            bytes: r.span(start),
            data,
            owner,
            previous_transaction,
            storage_rebate,
        })
    }

    pub fn parse_vec<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<&'a [Object<'a>]> {
        let n = r.seq_len(Object::MIN_WIRE_SIZE)?;
        let mut out = a.slice(n)?;
        for _ in 0..n {
            out.push(Object::parse(r, a)?);
        }
        Ok(out.finish())
    }
}

/// `GenesisObject::RawObject`, the only variant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenesisObject<'a> {
    pub data: Data<'a>,
    pub owner: Owner<'a>,
}

impl<'a> GenesisObject<'a> {
    pub const MIN_WIRE_SIZE: usize = 1 + 1 + (1 + 1 + 8 + 1) + 1;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<GenesisObject<'a>> {
        r.enter()?;
        let object = match r.variant()? {
            0 => GenesisObject {
                data: Data::parse(r, a)?,
                owner: Owner::parse(r, a)?,
            },
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "GenesisObject",
                    tag,
                });
            }
        };
        r.leave();
        Ok(object)
    }
}

// Mainnet p99 of arena over wire size: 0.77.
crate::impl_wire!(Object, guess = 13);
crate::impl_wire!(Owner);
