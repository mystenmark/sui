// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::AccountAddress;
use crate::type_tag as view;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    U8,
    U64,
    U128,
    Address,
    Signer,
    Vector(Box<TypeTag>),
    #[serde(rename = "struct")]
    Struct(Box<StructTag>),
    U16,
    U32,
    U256,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StructTag {
    pub address: AccountAddress,
    pub module: String,
    pub name: String,
    #[serde(rename = "type_args")]
    pub type_params: Vec<TypeTag>,
}

/// The same wire type as [`TypeTag`] under another name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TypeInput {
    #[serde(rename = "bool")]
    Bool,
    U8,
    U64,
    U128,
    Address,
    Signer,
    Vector(Box<TypeInput>),
    Struct(Box<StructInput>),
    U16,
    U32,
    U256,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StructInput {
    pub address: AccountAddress,
    pub module: String,
    pub name: String,
    #[serde(rename = "type_args")]
    pub type_params: Vec<TypeInput>,
}

impl From<&view::TypeTag<'_>> for TypeTag {
    fn from(v: &view::TypeTag<'_>) -> Self {
        match v {
            view::TypeTag::Bool => TypeTag::Bool,
            view::TypeTag::U8 => TypeTag::U8,
            view::TypeTag::U64 => TypeTag::U64,
            view::TypeTag::U128 => TypeTag::U128,
            view::TypeTag::Address => TypeTag::Address,
            view::TypeTag::Signer => TypeTag::Signer,
            view::TypeTag::Vector(inner) => TypeTag::Vector(Box::new(TypeTag::from(&**inner))),
            view::TypeTag::Struct(inner) => TypeTag::Struct(Box::new(StructTag::from(&**inner))),
            view::TypeTag::U16 => TypeTag::U16,
            view::TypeTag::U32 => TypeTag::U32,
            view::TypeTag::U256 => TypeTag::U256,
        }
    }
}

impl From<&view::StructTag<'_>> for StructTag {
    fn from(v: &view::StructTag<'_>) -> Self {
        StructTag {
            address: AccountAddress::from(v.address),
            module: v.module.to_owned(),
            name: v.name.to_owned(),
            type_params: v.type_params.iter().map(TypeTag::from).collect(),
        }
    }
}

// The views have one type for both, so the walk is done once as `TypeTag`.
impl From<&view::TypeInput<'_>> for TypeInput {
    fn from(v: &view::TypeInput<'_>) -> Self {
        TypeInput::from(TypeTag::from(v))
    }
}

impl From<&view::StructInput<'_>> for StructInput {
    fn from(v: &view::StructInput<'_>) -> Self {
        StructInput::from(StructTag::from(v))
    }
}

impl From<TypeTag> for TypeInput {
    fn from(t: TypeTag) -> Self {
        match t {
            TypeTag::Bool => TypeInput::Bool,
            TypeTag::U8 => TypeInput::U8,
            TypeTag::U64 => TypeInput::U64,
            TypeTag::U128 => TypeInput::U128,
            TypeTag::Address => TypeInput::Address,
            TypeTag::Signer => TypeInput::Signer,
            TypeTag::Vector(inner) => TypeInput::Vector(Box::new(TypeInput::from(*inner))),
            TypeTag::Struct(inner) => TypeInput::Struct(Box::new(StructInput::from(*inner))),
            TypeTag::U16 => TypeInput::U16,
            TypeTag::U32 => TypeInput::U32,
            TypeTag::U256 => TypeInput::U256,
        }
    }
}

impl From<StructTag> for StructInput {
    fn from(s: StructTag) -> Self {
        StructInput {
            address: s.address,
            module: s.module,
            name: s.name,
            type_params: s.type_params.into_iter().map(TypeInput::from).collect(),
        }
    }
}
