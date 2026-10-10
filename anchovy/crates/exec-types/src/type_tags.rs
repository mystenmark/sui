// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Between the Move VM's owned type tags and anchovy's views. The VM hands
//! out owned `TypeTag`s (and takes them back to load types); execution
//! keeps types as views in the transaction's arena.

use containers::{Bump, IndexSet, alloc, alloc_str};
use messages::arena::Ref;
use messages::base::AccountAddress;
use messages::object::MoveObjectType;
use messages::type_tag::{StructTag, TypeTag};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage as move_tags;

use crate::base::{SUI_FRAMEWORK_ADDRESS, SUI_SYSTEM_ADDRESS};

pub fn type_tag_in<'a>(bump: &'a Bump, tag: &move_tags::TypeTag) -> TypeTag<'a> {
    match tag {
        move_tags::TypeTag::Bool => TypeTag::Bool,
        move_tags::TypeTag::U8 => TypeTag::U8,
        move_tags::TypeTag::U16 => TypeTag::U16,
        move_tags::TypeTag::U32 => TypeTag::U32,
        move_tags::TypeTag::U64 => TypeTag::U64,
        move_tags::TypeTag::U128 => TypeTag::U128,
        move_tags::TypeTag::U256 => TypeTag::U256,
        move_tags::TypeTag::Address => TypeTag::Address,
        move_tags::TypeTag::Signer => TypeTag::Signer,
        move_tags::TypeTag::Vector(inner) => {
            TypeTag::Vector(Ref::new(alloc(bump, type_tag_in(bump, inner))))
        }
        move_tags::TypeTag::Struct(s) => {
            TypeTag::Struct(Ref::new(alloc(bump, struct_tag_in(bump, s))))
        }
    }
}

pub fn struct_tag_in<'a>(bump: &'a Bump, tag: &move_tags::StructTag) -> StructTag<'a> {
    let mut params = containers::Vec::with_capacity_in(tag.type_params.len(), bump);
    params.extend(tag.type_params.iter().map(|t| type_tag_in(bump, t)));
    StructTag {
        address: alloc(bump, AccountAddress(tag.address.into_bytes())),
        module: alloc_str(bump, tag.module.as_str()),
        name: alloc_str(bump, tag.name.as_str()),
        type_params: params.leak(),
    }
}

pub fn to_move_type_tag(tag: &TypeTag<'_>) -> move_tags::TypeTag {
    match tag {
        TypeTag::Bool => move_tags::TypeTag::Bool,
        TypeTag::U8 => move_tags::TypeTag::U8,
        TypeTag::U16 => move_tags::TypeTag::U16,
        TypeTag::U32 => move_tags::TypeTag::U32,
        TypeTag::U64 => move_tags::TypeTag::U64,
        TypeTag::U128 => move_tags::TypeTag::U128,
        TypeTag::U256 => move_tags::TypeTag::U256,
        TypeTag::Address => move_tags::TypeTag::Address,
        TypeTag::Signer => move_tags::TypeTag::Signer,
        TypeTag::Vector(inner) => move_tags::TypeTag::Vector(Box::new(to_move_type_tag(inner))),
        TypeTag::Struct(s) => move_tags::TypeTag::Struct(Box::new(to_move_struct_tag(s))),
    }
}

/// # Panics
/// If a module or name is not a Move identifier: anchovy's parsers do not
/// check them, but every type tag execution converts names a loaded type.
pub fn to_move_struct_tag(tag: &StructTag<'_>) -> move_tags::StructTag {
    move_tags::StructTag {
        address: move_core_types::account_address::AccountAddress::new(tag.address.0),
        module: Identifier::new(tag.module).expect("a loaded module's name"),
        name: Identifier::new(tag.name).expect("a loaded type's name"),
        type_params: tag.type_params.iter().map(to_move_type_tag).collect(),
    }
}

fn is(s: &move_tags::StructTag, address: &AccountAddress, module: &str, name: &str) -> bool {
    s.address.into_bytes() == address.0 && s.module.as_str() == module && s.name.as_str() == name
}

/// `GAS::is_gas_type`: `0x2::sui::SUI`.
fn is_gas_type(t: &move_tags::TypeTag) -> bool {
    matches!(t, move_tags::TypeTag::Struct(s)
        if is(s, &SUI_FRAMEWORK_ADDRESS, "sui", "SUI") && s.type_params.is_empty())
}

/// `accumulator_value_balance_type_maybe`: `T` if `s` is
/// `0x2::dynamic_field::Field<0x2::accumulator::Key<0x2::balance::Balance<T>>, 0x2::accumulator::U128>`.
fn accumulator_value_balance_type_maybe(s: &move_tags::StructTag) -> Option<&move_tags::TypeTag> {
    if !(is(s, &SUI_FRAMEWORK_ADDRESS, "dynamic_field", "Field") && s.type_params.len() == 2) {
        return None;
    }
    let move_tags::TypeTag::Struct(key) = &s.type_params[0] else {
        return None;
    };
    if !(is(key, &SUI_FRAMEWORK_ADDRESS, "accumulator", "Key") && key.type_params.len() == 1) {
        return None;
    }
    let is_u128 = matches!(&s.type_params[1], move_tags::TypeTag::Struct(v)
        if is(v, &SUI_FRAMEWORK_ADDRESS, "accumulator", "U128") && v.type_params.is_empty());
    if !is_u128 {
        return None;
    }
    // `Balance::maybe_get_balance_type_param`.
    match &key.type_params[0] {
        move_tags::TypeTag::Struct(b)
            if is(b, &SUI_FRAMEWORK_ADDRESS, "balance", "Balance") && b.type_params.len() == 1 =>
        {
            Some(&b.type_params[0])
        }
        _ => None,
    }
}

/// `MoveObjectType::from(StructTag)`: the type's compact form for the
/// framework's well-known types.
pub fn move_object_type_in<'a>(bump: &'a Bump, s: &move_tags::StructTag) -> MoveObjectType<'a> {
    let is_coin = is(s, &SUI_FRAMEWORK_ADDRESS, "coin", "Coin");
    if is_coin && s.type_params.len() == 1 && is_gas_type(&s.type_params[0]) {
        MoveObjectType::GasCoin
    } else if is_coin {
        // A coin has exactly one type parameter.
        MoveObjectType::Coin(type_tag_in(
            bump,
            s.type_params.last().expect("a coin has a type parameter"),
        ))
    } else if is(s, &SUI_SYSTEM_ADDRESS, "staking_pool", "StakedSui") && s.type_params.is_empty() {
        MoveObjectType::StakedSui
    } else if let Some(balance_type) = accumulator_value_balance_type_maybe(s) {
        if is_gas_type(balance_type) {
            MoveObjectType::SuiBalanceAccumulatorField
        } else {
            MoveObjectType::BalanceAccumulatorField(type_tag_in(bump, balance_type))
        }
    } else {
        MoveObjectType::Other(struct_tag_in(bump, s))
    }
}

fn view_is(s: &StructTag<'_>, address: &AccountAddress, module: &str, name: &str) -> bool {
    s.address == address && s.module == module && s.name == name
}

fn view_is_gas_type(t: &TypeTag<'_>) -> bool {
    matches!(t, TypeTag::Struct(s)
        if view_is(s, &SUI_FRAMEWORK_ADDRESS, "sui", "SUI") && s.type_params.is_empty())
}

/// `accumulator_value_balance_type_maybe`, for a view.
fn view_accumulator_value_balance_type_maybe<'a>(s: &StructTag<'a>) -> Option<TypeTag<'a>> {
    if !(view_is(s, &SUI_FRAMEWORK_ADDRESS, "dynamic_field", "Field") && s.type_params.len() == 2) {
        return None;
    }
    let TypeTag::Struct(key) = s.type_params[0] else {
        return None;
    };
    let key = key.get();
    if !(view_is(key, &SUI_FRAMEWORK_ADDRESS, "accumulator", "Key") && key.type_params.len() == 1) {
        return None;
    }
    let is_u128 = matches!(&s.type_params[1], TypeTag::Struct(v)
        if view_is(v, &SUI_FRAMEWORK_ADDRESS, "accumulator", "U128") && v.type_params.is_empty());
    if !is_u128 {
        return None;
    }
    // `Balance::maybe_get_balance_type_param`.
    match key.type_params[0] {
        TypeTag::Struct(b)
            if view_is(&b, &SUI_FRAMEWORK_ADDRESS, "balance", "Balance")
                && b.type_params.len() == 1 =>
        {
            Some(b.get().type_params[0])
        }
        _ => None,
    }
}

/// `MoveObjectType::from(StructTag)`, for a view: the compact form of the framework's well-known
/// types, borrowing the tag's parts.
pub fn move_object_type_of<'a>(s: &StructTag<'a>) -> MoveObjectType<'a> {
    let is_coin = view_is(s, &SUI_FRAMEWORK_ADDRESS, "coin", "Coin");
    if is_coin && s.type_params.len() == 1 && view_is_gas_type(&s.type_params[0]) {
        MoveObjectType::GasCoin
    } else if is_coin {
        // A coin has exactly one type parameter.
        MoveObjectType::Coin(*s.type_params.last().expect("a coin has a type parameter"))
    } else if view_is(s, &SUI_SYSTEM_ADDRESS, "staking_pool", "StakedSui")
        && s.type_params.is_empty()
    {
        MoveObjectType::StakedSui
    } else if let Some(balance_type) = view_accumulator_value_balance_type_maybe(s) {
        if view_is_gas_type(&balance_type) {
            MoveObjectType::SuiBalanceAccumulatorField
        } else {
            MoveObjectType::BalanceAccumulatorField(balance_type)
        }
    } else {
        MoveObjectType::Other(*s)
    }
}

/// `StructTag::from(MoveObjectType)`: the full type.
pub fn to_move_struct_tag_of(t: &MoveObjectType<'_>) -> move_tags::StructTag {
    let framework = move_core_types::account_address::AccountAddress::new(SUI_FRAMEWORK_ADDRESS.0);
    let tag = |address, module: &str, name: &str, type_params| move_tags::StructTag {
        address,
        module: Identifier::new(module).expect("a framework module"),
        name: Identifier::new(name).expect("a framework type"),
        type_params,
    };
    let gas = || move_tags::TypeTag::Struct(Box::new(tag(framework, "sui", "SUI", vec![])));
    let balance_field = |balance_type| {
        let balance = tag(framework, "balance", "Balance", vec![balance_type]);
        let key = tag(
            framework,
            "accumulator",
            "Key",
            vec![move_tags::TypeTag::Struct(Box::new(balance))],
        );
        let value = tag(framework, "accumulator", "U128", vec![]);
        tag(
            framework,
            "dynamic_field",
            "Field",
            vec![
                move_tags::TypeTag::Struct(Box::new(key)),
                move_tags::TypeTag::Struct(Box::new(value)),
            ],
        )
    };
    match t {
        MoveObjectType::GasCoin => tag(framework, "coin", "Coin", vec![gas()]),
        MoveObjectType::StakedSui => tag(
            move_core_types::account_address::AccountAddress::new(SUI_SYSTEM_ADDRESS.0),
            "staking_pool",
            "StakedSui",
            vec![],
        ),
        MoveObjectType::Coin(inner) => {
            tag(framework, "coin", "Coin", vec![to_move_type_tag(inner)])
        }
        MoveObjectType::SuiBalanceAccumulatorField => balance_field(gas()),
        MoveObjectType::BalanceAccumulatorField(inner) => balance_field(to_move_type_tag(inner)),
        MoveObjectType::Other(s) => to_move_struct_tag(s),
    }
}

/// `StructTag::from(MoveObjectType)`, in `bump`: the full type, as a view.
pub fn move_object_type_struct_tag_in<'a>(bump: &'a Bump, t: &MoveObjectType<'a>) -> StructTag<'a> {
    let framework: &'static AccountAddress = &SUI_FRAMEWORK_ADDRESS;
    let tag = |address, module, name, type_params: &[TypeTag<'a>]| StructTag {
        address,
        module,
        name,
        type_params: containers::alloc_slice_copy(bump, type_params),
    };
    let struct_ = |s: StructTag<'a>| TypeTag::Struct(Ref::new(alloc(bump, s)));
    let gas = || struct_(tag(framework, "sui", "SUI", &[]));
    let balance_field = |balance_type| {
        let balance = struct_(tag(framework, "balance", "Balance", &[balance_type]));
        let key = struct_(tag(framework, "accumulator", "Key", &[balance]));
        let value = struct_(tag(framework, "accumulator", "U128", &[]));
        tag(framework, "dynamic_field", "Field", &[key, value])
    };
    match t {
        MoveObjectType::GasCoin => tag(framework, "coin", "Coin", &[gas()]),
        MoveObjectType::StakedSui => tag(&SUI_SYSTEM_ADDRESS, "staking_pool", "StakedSui", &[]),
        MoveObjectType::Coin(inner) => tag(framework, "coin", "Coin", &[*inner]),
        MoveObjectType::SuiBalanceAccumulatorField => balance_field(gas()),
        MoveObjectType::BalanceAccumulatorField(inner) => balance_field(*inner),
        MoveObjectType::Other(s) => *s,
    }
}

/// `TypeTag::all_addresses`: every address in the type, in pre-order.
pub fn all_addresses<'a>(
    bump: &'a Bump,
    tag: &TypeTag<'_>,
) -> IndexSet<'a, move_core_types::account_address::AccountAddress> {
    let mut addresses = IndexSet::new_in(bump);
    find_addresses(tag, &mut addresses);
    addresses
}

/// `StructTag::all_addresses`: the struct's address, then its type parameters', in pre-order.
pub fn struct_all_addresses<'a>(
    bump: &'a Bump,
    tag: &StructTag<'_>,
) -> IndexSet<'a, move_core_types::account_address::AccountAddress> {
    let mut addresses = IndexSet::new_in(bump);
    struct_addresses(tag, &mut addresses);
    addresses
}

fn find_addresses(
    tag: &TypeTag<'_>,
    addresses: &mut IndexSet<'_, move_core_types::account_address::AccountAddress>,
) {
    match tag {
        TypeTag::Bool
        | TypeTag::U8
        | TypeTag::U64
        | TypeTag::U128
        | TypeTag::U16
        | TypeTag::U32
        | TypeTag::U256
        | TypeTag::Address
        | TypeTag::Signer => (),
        TypeTag::Vector(inner) => find_addresses(inner, addresses),
        TypeTag::Struct(s) => struct_addresses(s, addresses),
    }
}

fn struct_addresses(
    tag: &StructTag<'_>,
    addresses: &mut IndexSet<'_, move_core_types::account_address::AccountAddress>,
) {
    // Traverse in a pre-order manner. So the address is added first, then the type parameters.
    addresses.insert(move_core_types::account_address::AccountAddress::new(
        tag.address.0,
    ));
    for param in tag.type_params {
        find_addresses(param, addresses);
    }
}

/// `StructTag::from(MoveObjectType).all_addresses()`, without building the full type: the
/// framework's types expand to addresses 0x2 and 0x3 around their type parameters.
pub fn move_object_type_all_addresses<'a>(
    bump: &'a Bump,
    ty: &MoveObjectType<'_>,
) -> IndexSet<'a, move_core_types::account_address::AccountAddress> {
    let framework = move_core_types::account_address::AccountAddress::new(SUI_FRAMEWORK_ADDRESS.0);
    let mut addresses = IndexSet::new_in(bump);
    match ty {
        MoveObjectType::Other(s) => struct_addresses(s, &mut addresses),
        // 0x2::coin::Coin<0x2::sui::SUI>, and the SUI accumulator field, are all framework types.
        MoveObjectType::GasCoin | MoveObjectType::SuiBalanceAccumulatorField => {
            addresses.insert(framework);
        }
        MoveObjectType::StakedSui => {
            addresses.insert(move_core_types::account_address::AccountAddress::new(
                SUI_SYSTEM_ADDRESS.0,
            ));
        }
        // 0x2::coin::Coin<T>, and 0x2::dynamic_field::Field<0x2::accumulator::Key<
        // 0x2::balance::Balance<T>>, 0x2::accumulator::U128>: 0x2, then T's.
        MoveObjectType::Coin(t) | MoveObjectType::BalanceAccumulatorField(t) => {
            addresses.insert(framework);
            find_addresses(t, &mut addresses);
        }
    }
    addresses
}

/// `TypeInput::to_type_tag`'s failure, without building the tag: the first module or type name
/// that is not a Move identifier. Views do not check names when parsed; the reference's
/// conversion does.
pub fn check_type_input(tag: &TypeTag<'_>) -> Result<(), String> {
    match tag {
        TypeTag::Vector(inner) => check_type_input(inner),
        TypeTag::Struct(s) => {
            for name in [s.module, s.name] {
                if !move_core_types::identifier::is_valid(name) {
                    return Err(format!("Invalid identifier: {name}"));
                }
            }
            s.type_params.iter().try_for_each(check_type_input)
        }
        _ => Ok(()),
    }
}
