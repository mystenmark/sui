// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::accumulator_root` (and `AccumulatorEvent::from_balance_change`)
//! execution uses: balance accumulator fields under the accumulator root.

use containers::Bump;
use exec_types::base::{SUI_ACCUMULATOR_ROOT_OBJECT_ID, SUI_FRAMEWORK_ADDRESS};
use exec_types::storage::{RuntimeObjectResolver, SuiError, SuiResult};
use exec_types::type_tags::to_move_type_tag;
use messages::base::{ObjectId, SequenceNumber, SuiAddress};
use messages::effects::{AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1};
use messages::type_tag::TypeTag;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage as move_tags;

use crate::accumulator_event::AccumulatorEvent;

/// `SequenceNumber::MAX`, the bound of an unbounded read.
const SEQUENCE_NUMBER_MAX: SequenceNumber = 0x7fff_ffff_ffff_ffff;

/// `Balance::is_balance_type`: `0x2::balance::Balance<T>`.
pub fn is_balance_type(ty: &TypeTag<'_>) -> bool {
    matches!(ty, TypeTag::Struct(s)
        if *s.address == SUI_FRAMEWORK_ADDRESS
            && s.module == "balance"
            && s.name == "Balance"
            && s.type_params.len() == 1)
}

/// `Balance::maybe_get_balance_type_param`.
pub fn maybe_get_balance_type_param<'a>(ty: &TypeTag<'a>) -> Option<TypeTag<'a>> {
    match ty {
        TypeTag::Struct(s) if is_balance_type(ty) => Some(s.type_params[0]),
        _ => None,
    }
}

/// `AccumulatorKey::get_type_tag`: `0x2::accumulator::Key<T>`, as the VM's type tag the field id
/// hashes.
fn accumulator_key_type_tag(type_: &TypeTag<'_>) -> move_tags::TypeTag {
    move_tags::TypeTag::Struct(Box::new(move_tags::StructTag {
        address: exec_types::base::move_address(&ObjectId(SUI_FRAMEWORK_ADDRESS.0)),
        module: Identifier::new("accumulator").expect("an identifier"),
        name: Identifier::new("Key").expect("an identifier"),
        type_params: vec![to_move_type_tag(type_)],
    }))
}

/// `AccumulatorValue::get_field_id`: the id of `owner`'s accumulator field for `type_`.
pub fn get_field_id(owner: &SuiAddress, type_: &TypeTag<'_>) -> SuiResult<ObjectId> {
    if !is_balance_type(type_) {
        return Err(SuiError(
            "TypeError: only Balance<T> is supported".to_string(),
        ));
    }
    // The key is `AccumulatorKey { owner }`, whose BCS is the owner's bytes.
    let id = sui_types::dynamic_field::derive_dynamic_field_id(
        sui_types::base_types::ObjectID::new(SUI_ACCUMULATOR_ROOT_OBJECT_ID.0),
        &accumulator_key_type_tag(type_),
        &owner.0,
    )
    .map_err(|e| SuiError(format!("DynamicFieldReadError: {e}")))?;
    Ok(ObjectId(id.into_bytes()))
}

/// `AccumulatorValue::load`: the `U128` value of `owner`'s field for `type_`, as of
/// `version_bound` on the root, if the field exists.
pub fn load<'a>(
    runtime_object_resolver: &dyn RuntimeObjectResolver<'a>,
    version_bound: Option<SequenceNumber>,
    owner: &SuiAddress,
    type_: &TypeTag<'_>,
) -> SuiResult<Option<u128>> {
    let id = get_field_id(owner, type_)?;
    let Some(object) = runtime_object_resolver.read_child_object(
        &SUI_ACCUMULATOR_ROOT_OBJECT_ID,
        &id,
        version_bound.unwrap_or(SEQUENCE_NUMBER_MAX),
    )?
    else {
        return Ok(None);
    };
    let move_object = object.try_as_move().ok_or_else(|| {
        SuiError(format!(
            "DynamicFieldReadError: Dynamic field {:?} is not a Move object",
            object.id()
        ))
    })?;
    // `Field<AccumulatorKey, U128>`: the field's UID, the key's owner, then the u128.
    let contents = move_object.contents;
    let value: [u8; 16] = match contents.len() {
        80 => contents[64..80].try_into().expect("sixteen bytes"),
        n => {
            return Err(SuiError(format!(
                "DynamicFieldReadError: accumulator field of {n} bytes"
            )));
        }
    };
    Ok(Some(u128::from_le_bytes(value)))
}

/// `AccumulatorEvent::from_balance_change`: a Merge (deposit) or Split (withdrawal) of
/// `net_change` to `address`'s balance of `balance_type` (a `Balance<T>`).
pub fn from_balance_change<'a>(
    bump: &'a Bump,
    address: SuiAddress,
    balance_type: TypeTag<'a>,
    net_change: i64,
) -> SuiResult<AccumulatorEvent<'a>> {
    if !is_balance_type(&balance_type) {
        return Err(SuiError(
            "TypeError: only Balance<T> is supported".to_string(),
        ));
    }
    let accumulator_obj = get_field_id(&address, &balance_type)?;

    let (operation, amount) = if net_change < 0 {
        (AccumulatorOperation::Split, net_change.unsigned_abs())
    } else {
        (AccumulatorOperation::Merge, net_change as u64)
    };

    let accumulator_write = AccumulatorWriteV1 {
        address: containers::alloc(bump, address),
        ty: balance_type,
        operation,
        value: AccumulatorValue::Integer(amount),
    };

    Ok(AccumulatorEvent::new(accumulator_obj, accumulator_write))
}

/// `Balance::type_tag(inner)`: `0x2::balance::Balance<inner>`, in `bump`.
pub fn balance_type<'a>(bump: &'a Bump, inner: TypeTag<'a>) -> TypeTag<'a> {
    let address = containers::alloc(
        bump,
        messages::base::AccountAddress(SUI_FRAMEWORK_ADDRESS.0),
    );
    TypeTag::Struct(messages::arena::Ref::new(containers::alloc(
        bump,
        messages::type_tag::StructTag {
            address,
            module: "balance",
            name: "Balance",
            type_params: containers::alloc_slice_copy(bump, &[inner]),
        },
    )))
}

/// `GAS::type_tag()`: `0x2::sui::SUI`, in `bump`.
pub fn sui_type(bump: &Bump) -> TypeTag<'_> {
    let address = containers::alloc(
        bump,
        messages::base::AccountAddress(SUI_FRAMEWORK_ADDRESS.0),
    );
    TypeTag::Struct(messages::arena::Ref::new(containers::alloc(
        bump,
        messages::type_tag::StructTag {
            address,
            module: "sui",
            name: "SUI",
            type_params: &[],
        },
    )))
}

/// `Balance::type_tag(GAS::type_tag())`: `0x2::balance::Balance<0x2::sui::SUI>`, built once per
/// call in `bump`.
pub fn sui_balance_type(bump: &Bump) -> TypeTag<'_> {
    balance_type(bump, sui_type(bump))
}
