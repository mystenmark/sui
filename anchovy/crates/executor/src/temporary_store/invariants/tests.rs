// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The invariant checks over hand-built stores. Checks that report through
//! `make_invariant_violation!` panic in debug builds, as the reference's do.

use std::collections::BTreeMap as StdBTreeMap;

use containers::{BTreeSet, Bump, Vec};
use exec_types::base::{EpochId, SUI_FRAMEWORK_ADDRESS};
use exec_types::object::Object;
use exec_types::storage::{
    BackingPackageStore, ObjectStore, RuntimeObjectResolver, SuiError, SuiResult,
};
use messages::base::{AccountAddress, Digest, ObjectId, SequenceNumber, SuiAddress, U64Le};
use messages::effects::GasCostSummary;
use messages::object::{Linkage, MoveObject, MoveObjectType, MovePackage, Owner};
use messages::type_tag::{StructTag, TypeTag};
use move_core_types::annotated_value::{
    MoveDatatypeLayout, MoveFieldLayout, MoveStructLayout, MoveTypeLayout,
};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage as move_tags;
use sui_protocol_config::ProtocolConfig;

use super::{LayoutResolver, get_total_sui};
use crate::accumulator_root::{from_balance_change, sui_balance_type};
use crate::gas_charger::GasCharger;
use crate::inputs::{ExecutionInputs, InputObjectKind, InputState, LoadedInput};
use crate::temporary_store::TemporaryStore;

#[derive(Default)]
struct TestStore<'a> {
    objects: StdBTreeMap<ObjectId, Object<'a>>,
}

impl<'a> BackingPackageStore<'a> for TestStore<'a> {
    fn get_package_object(&self, package_id: &ObjectId) -> SuiResult<Option<Object<'a>>> {
        Ok(self
            .objects
            .get(package_id)
            .filter(|o| o.is_package())
            .copied())
    }
}

impl<'a> RuntimeObjectResolver<'a> for TestStore<'a> {
    fn read_child_object(
        &self,
        _parent: &ObjectId,
        _child: &ObjectId,
        _child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object<'a>>> {
        Ok(None)
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectId,
        _receiving_object_id: &ObjectId,
        _receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object<'a>>> {
        Ok(None)
    }
}

impl<'a> ObjectStore<'a> for TestStore<'a> {
    fn get_object(&self, object_id: &ObjectId) -> Option<Object<'a>> {
        self.objects.get(object_id).copied()
    }

    fn get_object_by_key(
        &self,
        object_id: &ObjectId,
        version: SequenceNumber,
    ) -> Option<Object<'a>> {
        self.get_object(object_id)
            .filter(|o| o.version() == version)
    }
}

/// A layout resolver for stores of coins, which need none.
struct NoLayouts;

impl LayoutResolver for NoLayouts {
    fn get_annotated_layout(
        &mut self,
        struct_tag: &move_tags::StructTag,
    ) -> Result<MoveDatatypeLayout, SuiError> {
        Err(SuiError(format!("no layout for {struct_tag}")))
    }
}

fn address(n: u16) -> SuiAddress {
    SuiAddress(ObjectId::from_u16(n).0)
}

fn address_owner(bump: &Bump, n: u16) -> Owner<'_> {
    Owner::AddressOwner(containers::alloc(bump, address(n)))
}

fn object_owner(bump: &Bump, id: ObjectId) -> Owner<'_> {
    Owner::ObjectOwner(containers::alloc(bump, SuiAddress(id.0)))
}

/// A sealed object of type `type_`, whose contents start with its id.
fn move_object<'a>(
    bump: &'a Bump,
    id: ObjectId,
    version: SequenceNumber,
    type_: MoveObjectType<'a>,
    rest: &[u8],
    owner: Owner<'a>,
    storage_rebate: u64,
) -> Object<'a> {
    let mut contents = Vec::new_in(bump);
    contents.extend_from_slice(&id.0);
    contents.extend_from_slice(rest);
    let contents = containers::alloc_slice_copy(bump, &contents);
    Object::new_move(
        MoveObject {
            type_,
            has_public_transfer: true,
            version,
            contents,
        },
        owner,
        Digest::new([7; 32]),
    )
    .with_storage_rebate(storage_rebate)
    .seal(bump)
}

fn gas_coin<'a>(
    bump: &'a Bump,
    id: ObjectId,
    version: SequenceNumber,
    value: u64,
    owner: Owner<'a>,
    storage_rebate: u64,
) -> Object<'a> {
    move_object(
        bump,
        id,
        version,
        MoveObjectType::GasCoin,
        &value.to_le_bytes(),
        owner,
        storage_rebate,
    )
}

fn owned_input(object: Object<'_>) -> LoadedInput<'_> {
    LoadedInput::new(
        InputObjectKind::ImmOrOwnedMoveObject(object.compute_object_reference()),
        InputState::Object(object),
    )
}

/// A store over `inputs`, backed by `backing`, with no transaction-derived reservations.
fn store<'a>(
    bump: &'a Bump,
    inputs: &[LoadedInput<'a>],
    backing: &[Object<'a>],
) -> TemporaryStore<'a> {
    let mut backing_store = TestStore::default();
    for object in inputs
        .iter()
        .filter_map(LoadedInput::as_object)
        .chain(backing)
    {
        backing_store.objects.insert(object.id(), *object);
    }
    let backing_store = containers::leak(bump, backing_store);
    let inputs = containers::leak(
        bump,
        ExecutionInputs::new(bump, containers::vec_from_slice(bump, inputs), &[], None),
    );
    let protocol_config = containers::leak(bump, ProtocolConfig::get_for_max_version_UNSAFE());
    TemporaryStore::new_for_genesis_state_update(
        bump,
        backing_store,
        inputs,
        Digest::new([1; 32]),
        protocol_config,
    )
}

fn summary(
    computation_cost: u64,
    storage_cost: u64,
    storage_rebate: u64,
    non_refundable_storage_fee: u64,
) -> GasCostSummary {
    GasCostSummary {
        computation_cost,
        storage_cost,
        storage_rebate,
        non_refundable_storage_fee,
    }
}

/// A gas coin of 1000 MIST with a storage rebate of 100, charged 10 for computation and 120 for
/// storage, refunded 99 of its rebate: 969 left.
fn charged_coin_store(bump: &Bump, value_after: u64) -> TemporaryStore<'_> {
    let coin = gas_coin(
        bump,
        ObjectId::from_u16(0x100),
        3,
        1000,
        address_owner(bump, 1),
        100,
    );
    let mut store = store(bump, &[owned_input(coin)], &[]);
    let charged = gas_coin(bump, coin.id(), 3, value_after, *coin.owner(), 120);
    store.mutate_input_object(charged);
    store
}

#[test]
fn sui_conserved_with_storage_charges() {
    let bump = Bump::with_capacity(1 << 16);
    let store = charged_coin_store(&bump, 969);
    let checker = &store.invariants;
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(10, 120, 99, 1))
            .is_ok()
    );

    // The rebate of the input must go to the summary's rebates.
    let err = checker
        .check_sui_conserved(&store, true, &summary(10, 120, 98, 1))
        .unwrap_err();
    assert_eq!(
        *err.kind(),
        messages::execution_status::ExecutionErrorKind::InvariantViolation
    );
    // The storage charge must go to the outputs' rebates.
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(10, 121, 99, 1))
            .is_err()
    );
    // Off, nothing is checked.
    assert!(
        checker
            .check_sui_conserved(&store, false, &summary(10, 121, 99, 1))
            .is_ok()
    );
}

#[test]
fn sui_conserved_without_storage_charges() {
    let bump = Bump::with_capacity(1 << 16);
    let kept = gas_coin(
        &bump,
        ObjectId::from_u16(0x100),
        3,
        1000,
        address_owner(&bump, 1),
        100,
    );
    let deleted = gas_coin(
        &bump,
        ObjectId::from_u16(0x101),
        4,
        5,
        address_owner(&bump, 1),
        30,
    );
    let mut store = store(&bump, &[owned_input(kept), owned_input(deleted)], &[]);
    store.mutate_input_object(kept);
    store.delete_input_object(&deleted.id());
    let checker = &store.invariants;
    // Inputs 130 = outputs 100 + rebate 29 + non-refundable 1.
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(0, 0, 29, 1))
            .is_ok()
    );
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(0, 0, 28, 1))
            .is_err()
    );
}

#[test]
fn created_objects_count_as_outputs() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    let created = gas_coin(
        &bump,
        ObjectId::from_u16(0x200),
        0,
        0,
        address_owner(&bump, 1),
        50,
    );
    store.create_object(created);
    let checker = &store.invariants;
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(0, 50, 0, 0))
            .is_ok()
    );
    assert!(
        checker
            .check_sui_conserved(&store, true, &summary(0, 49, 0, 0))
            .is_err()
    );
}

#[test]
fn sui_conserved_expensive() {
    let bump = Bump::with_capacity(1 << 16);
    let store = charged_coin_store(&bump, 969);
    // In 1000 + 100; out 969 + 120, computation 10, non-refundable 1.
    assert!(
        store
            .invariants
            .check_sui_conserved_expensive(&store, &summary(10, 120, 99, 1), &mut NoLayouts)
            .is_ok()
    );

    let store = charged_coin_store(&bump, 970);
    assert!(
        store
            .invariants
            .check_sui_conserved_expensive(&store, &summary(10, 120, 99, 1), &mut NoLayouts)
            .is_err()
    );
}

#[test]
fn sui_conserved_expensive_counts_balance_events() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = charged_coin_store(&bump, 969 - 400);
    // 400 MIST of the coin deposited into an address balance.
    store.add_accumulator_event(
        from_balance_change(&bump, address(9), sui_balance_type(&bump), 400).unwrap(),
    );
    assert!(
        store
            .invariants
            .check_sui_conserved_expensive(&store, &summary(10, 120, 99, 1), &mut NoLayouts)
            .is_ok()
    );
}

/// `0x42::m::Wrapper { id: address, sui: Balance<SUI>, other: Balance<0x42::m::T>, n: u64 }`.
struct WrapperLayout;

fn framework_struct(
    module: &str,
    name: &str,
    type_params: std::vec::Vec<move_tags::TypeTag>,
) -> move_tags::StructTag {
    move_tags::StructTag {
        address: exec_types::base::move_address(&ObjectId(SUI_FRAMEWORK_ADDRESS.0)),
        module: Identifier::new(module).unwrap(),
        name: Identifier::new(name).unwrap(),
        type_params,
    }
}

fn balance_layout(coin: move_tags::TypeTag) -> MoveTypeLayout {
    MoveTypeLayout::Struct(Box::new(MoveStructLayout::new(
        framework_struct("balance", "Balance", vec![coin]),
        vec![MoveFieldLayout::new(
            Identifier::new("value").unwrap(),
            MoveTypeLayout::U64,
        )],
    )))
}

impl LayoutResolver for WrapperLayout {
    fn get_annotated_layout(
        &mut self,
        struct_tag: &move_tags::StructTag,
    ) -> Result<MoveDatatypeLayout, SuiError> {
        let other = move_tags::TypeTag::Struct(Box::new(move_tags::StructTag {
            name: Identifier::new("T").unwrap(),
            type_params: vec![],
            ..struct_tag.clone()
        }));
        let field =
            |name: &str, layout| MoveFieldLayout::new(Identifier::new(name).unwrap(), layout);
        Ok(MoveDatatypeLayout::Struct(Box::new(MoveStructLayout::new(
            struct_tag.clone(),
            vec![
                field("id", MoveTypeLayout::Address),
                field("sui", balance_layout(sui_types::gas_coin::GAS::type_tag())),
                field("other", balance_layout(other)),
                field("n", MoveTypeLayout::U64),
            ],
        ))))
    }
}

#[test]
fn total_sui_walks_balances() {
    let bump = Bump::with_capacity(1 << 16);
    let type_ = MoveObjectType::Other(StructTag {
        address: containers::alloc(&bump, AccountAddress(ObjectId::from_u16(0x42).0)),
        module: "m",
        name: "Wrapper",
        type_params: &[],
    });
    let mut rest = std::vec::Vec::new();
    rest.extend_from_slice(&700u64.to_le_bytes());
    rest.extend_from_slice(&5u64.to_le_bytes());
    rest.extend_from_slice(&9u64.to_le_bytes());
    let wrapper = move_object(
        &bump,
        ObjectId::from_u16(0x300),
        1,
        type_,
        &rest,
        address_owner(&bump, 1),
        40,
    );
    // The rebate and the `Balance<SUI>`, not the other balance or the u64.
    assert_eq!(
        get_total_sui(&wrapper, &bump, &mut WrapperLayout).unwrap(),
        740
    );
    assert!(get_total_sui(&wrapper, &bump, &mut NoLayouts).is_err());

    // A coin of another type holds no SUI; a SUI balance field holds its value.
    let other_coin = move_object(
        &bump,
        ObjectId::from_u16(0x301),
        1,
        MoveObjectType::Coin(TypeTag::U8),
        &3u64.to_le_bytes(),
        address_owner(&bump, 1),
        2,
    );
    assert_eq!(
        get_total_sui(&other_coin, &bump, &mut NoLayouts).unwrap(),
        2
    );
    let mut field = std::vec::Vec::new();
    field.extend_from_slice(&address(1).0);
    field.extend_from_slice(&77u128.to_le_bytes());
    let balance_field = move_object(
        &bump,
        ObjectId::from_u16(0x302),
        1,
        MoveObjectType::SuiBalanceAccumulatorField,
        &field,
        object_owner(&bump, ObjectId::from_u16(0xacc)),
        0,
    );
    assert_eq!(
        get_total_sui(&balance_field, &bump, &mut NoLayouts).unwrap(),
        77
    );
}

#[test]
fn ownership_of_mutated_inputs_and_children() {
    let bump = Bump::with_capacity(1 << 16);
    let sender = address(1);
    let parent = gas_coin(
        &bump,
        ObjectId::from_u16(0x100),
        3,
        1,
        address_owner(&bump, 1),
        0,
    );
    let child = gas_coin(
        &bump,
        ObjectId::from_u16(0x101),
        2,
        1,
        object_owner(&bump, parent.id()),
        0,
    );
    let mut store = store(&bump, &[owned_input(parent)], &[child]);
    store.mutate_input_object(parent);
    store.mutate_child_object(child, child);
    let gas_charger = GasCharger::new_unmetered(Digest::ZERO, store.protocol_config());
    store
        .check_ownership_invariants(&sender, &None, &gas_charger, false)
        .unwrap();
}

#[test]
#[should_panic(expected = "Input object must be owned by sender")]
fn ownership_of_inputs_owned_by_others() {
    let bump = Bump::with_capacity(1 << 16);
    let coin = gas_coin(
        &bump,
        ObjectId::from_u16(0x100),
        3,
        1,
        address_owner(&bump, 2),
        0,
    );
    let store = store(&bump, &[owned_input(coin)], &[]);
    let gas_charger = GasCharger::new_unmetered(Digest::ZERO, store.protocol_config());
    let _ = store.check_ownership_invariants(&address(1), &None, &gas_charger, false);
}

#[test]
#[should_panic(expected = "Failed to load object")]
fn ownership_of_children_of_unauthenticated_parents() {
    let bump = Bump::with_capacity(1 << 16);
    let child = gas_coin(
        &bump,
        ObjectId::from_u16(0x101),
        2,
        1,
        object_owner(&bump, ObjectId::from_u16(0x999)),
        0,
    );
    let mut store = store(&bump, &[], &[child]);
    store.mutate_child_object(child, child);
    let gas_charger = GasCharger::new_unmetered(Digest::ZERO, store.protocol_config());
    let _ = store.check_ownership_invariants(&address(1), &None, &gas_charger, false);
}

#[test]
#[should_panic(expected = "Unauthenticated funds-accumulator Split")]
fn ownership_of_unreserved_balance_withdrawals() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    store.add_accumulator_event(
        from_balance_change(&bump, address(1), sui_balance_type(&bump), -5).unwrap(),
    );
    let gas_charger = GasCharger::new_unmetered(Digest::ZERO, store.protocol_config());
    let _ = store.check_ownership_invariants(&address(1), &None, &gas_charger, false);
}

#[test]
fn address_balance_changes_emitted_by_the_ptb() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    let sui = sui_balance_type(&bump);
    store.add_accumulator_event(from_balance_change(&bump, address(1), sui, -5).unwrap());
    store.add_accumulator_event(from_balance_change(&bump, address(2), sui, 5).unwrap());
    store.invariants.record_ptb_event_range(0, 2);
    // A runtime withdrawal covered by the PTB's deposit.
    store.add_accumulator_event(from_balance_change(&bump, address(2), sui, -5).unwrap());
    store
        .invariants
        .check_address_balance_changes(&store)
        .unwrap();
}

#[test]
#[should_panic(expected = "do not cover the runtime withdrawal")]
fn address_balance_changes_uncovered_by_the_ptb() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    let sui = sui_balance_type(&bump);
    store.add_accumulator_event(from_balance_change(&bump, address(2), sui, 5).unwrap());
    store.invariants.record_ptb_event_range(0, 1);
    store.add_accumulator_event(from_balance_change(&bump, address(2), sui, -6).unwrap());
    let _ = store.invariants.check_address_balance_changes(&store);
}

#[test]
#[should_panic(expected = "Unauthorized runtime Balance accumulator event")]
fn address_balance_changes_from_the_runtime() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    store.add_accumulator_event(
        from_balance_change(&bump, address(1), sui_balance_type(&bump), 5).unwrap(),
    );
    let _ = store.invariants.check_address_balance_changes(&store);
}

fn package<'a>(bump: &'a Bump, id: u16, modules: usize, deps: &[u16]) -> Object<'a> {
    let module_map: std::vec::Vec<(&str, &[u8])> = (0..modules).map(|_| ("m", &[][..])).collect();
    let linkage_table: std::vec::Vec<Linkage> = deps
        .iter()
        .map(|d| Linkage {
            original_id: ObjectId::from_u16(*d),
            upgraded_id: ObjectId::from_u16(*d),
            upgraded_version: U64Le::new(1),
        })
        .collect();
    Object::new_from_package(
        MovePackage {
            id: containers::alloc(bump, ObjectId::from_u16(id)),
            version: 1,
            module_map: containers::alloc_slice_copy(bump, &module_map),
            type_origin_table: &[],
            linkage_table: containers::alloc_slice_copy(bump, &linkage_table),
        },
        Digest::ZERO,
    )
}

fn declared<'a>(
    bump: &'a Bump,
    shapes: &[(usize, &[u16])],
) -> Vec<'a, (usize, BTreeSet<'a, ObjectId>)> {
    let mut declared = Vec::new_in(bump);
    for (modules, deps) in shapes {
        let mut ids = BTreeSet::new_in(bump);
        ids.extend(deps.iter().map(|d| ObjectId::from_u16(*d)));
        declared.push((*modules, ids));
    }
    declared
}

#[test]
fn published_packages_match_their_commands() {
    let bump = Bump::with_capacity(1 << 16);
    let mut store = store(&bump, &[], &[]);
    // Not a PTB: nothing to check.
    store.check_published_packages().unwrap();

    store.create_object(package(&bump, 0x500, 1, &[1, 2]));
    store.create_object(package(&bump, 0x501, 2, &[2]));
    store.post_execution_check_inputs.declared_packages =
        Some(declared(&bump, &[(2, &[2]), (1, &[2, 1])]));
    store.check_published_packages().unwrap();

    store.post_execution_check_inputs.declared_packages =
        Some(declared(&bump, &[(2, &[2]), (1, &[2])]));
    assert!(store.check_published_packages().is_err());
    store.post_execution_check_inputs.declared_packages = Some(declared(&bump, &[(2, &[2])]));
    assert!(store.check_published_packages().is_err());
}
