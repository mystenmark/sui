// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The execution-time coin deny list check against sui-types' over the same
//! deny list state: per-coin-type configs with denied addresses and global
//! pauses at epoch boundaries, read through one store that serves both the
//! reference's objects and anchovy's views of their BCS.
//!
//! The state is built with sui-types' `DynamicFieldKey::into_field`, which
//! produces the field objects' Move layout and ids; every scenario that
//! expects a denial checks that the reference reports it, so the objects are
//! found and parsed by the reference itself.

use std::collections::BTreeMap as StdBTreeMap;
use std::collections::BTreeSet as StdBTreeSet;

use containers::{BTreeMap, BTreeSet, Bump};
use exec_types::error::ExecutionErrorKind;
use messages::Message;
use move_core_types::account_address::AccountAddress;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::{StructTag, TypeTag};
use serde::{Deserialize, Serialize};
use sui_types::base_types::{MoveObjectType, ObjectID, SequenceNumber, SuiAddress};
use sui_types::config::{Setting, SettingData, setting_type};
use sui_types::digests::TransactionDigest;
use sui_types::dynamic_field::{DOFWrapper, DynamicFieldKey};
use sui_types::execution_status::ExecutionErrorKind as RefKind;
use sui_types::id::{ID, UID};
use sui_types::object::{MoveObject, Object, Owner};
use sui_types::{MoveTypeTagTrait, SUI_DENY_LIST_OBJECT_ID, SUI_FRAMEWORK_ADDRESS};

fn framework_struct(module: &str, name: &str, type_params: Vec<TypeTag>) -> StructTag {
    StructTag {
        address: SUI_FRAMEWORK_ADDRESS,
        module: Identifier::new(module).unwrap(),
        name: Identifier::new(name).unwrap(),
        type_params,
    }
}

/// `0x2::deny_list::ConfigKey`.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ConfigKey {
    per_type_index: u64,
    per_type_key: Vec<u8>,
}

impl MoveTypeTagTrait for ConfigKey {
    fn get_type_tag() -> TypeTag {
        TypeTag::Struct(Box::new(framework_struct("deny_list", "ConfigKey", vec![])))
    }
}

/// `0x2::deny_list::AddressKey`.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AddressKey(SuiAddress);

impl MoveTypeTagTrait for AddressKey {
    fn get_type_tag() -> TypeTag {
        TypeTag::Struct(Box::new(framework_struct(
            "deny_list",
            "AddressKey",
            vec![],
        )))
    }
}

/// `0x2::deny_list::GlobalPauseKey`: an empty struct, one zero byte.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct GlobalPauseKey(bool);

impl MoveTypeTagTrait for GlobalPauseKey {
    fn get_type_tag() -> TypeTag {
        TypeTag::Struct(Box::new(framework_struct(
            "deny_list",
            "GlobalPauseKey",
            vec![],
        )))
    }
}

/// `0x2::config::Setting<bool>`, which sui-types gives no type tag (bool has none).
#[derive(Debug, Serialize, Deserialize)]
struct BoolSetting(Setting<bool>);

impl MoveTypeTagTrait for BoolSetting {
    fn get_type_tag() -> TypeTag {
        TypeTag::Struct(Box::new(setting_type(TypeTag::Bool)))
    }
}

fn digest() -> TransactionDigest {
    TransactionDigest::new([7; 32])
}

fn move_object(type_: StructTag, version: u64, contents: Vec<u8>) -> MoveObject {
    // SAFETY: test objects, never written to a real store.
    unsafe {
        MoveObject::new_from_execution_with_limit(
            MoveObjectType::from(type_),
            false,
            SequenceNumber::from_u64(version),
            contents,
            1 << 20,
        )
    }
    .unwrap()
}

fn field_object<K, V>(parent: ObjectID, key: K, value: V, version: u64) -> Object
where
    K: Serialize + std::fmt::Debug + MoveTypeTagTrait,
    V: Serialize + serde::de::DeserializeOwned + MoveTypeTagTrait,
{
    let field = DynamicFieldKey(parent, key, K::get_type_tag())
        .into_field(value)
        .unwrap()
        .into_move_object_unsafe_for_testing(SequenceNumber::from_u64(version))
        .unwrap();
    Object::new_move(field, Owner::ObjectOwner(parent.into()), digest())
}

/// A setting's value at `newer_value_epoch`, as `0x2::config` stores it.
#[derive(Clone, Copy, Debug)]
struct SettingSpec {
    /// `None` for a `Setting` whose `data` is `None`.
    data: Option<(u64, Option<bool>, Option<bool>)>,
}

impl SettingSpec {
    fn new(newer_value_epoch: u64, newer: Option<bool>, older: Option<bool>) -> Self {
        Self {
            data: Some((newer_value_epoch, newer, older)),
        }
    }

    fn setting(self) -> BoolSetting {
        BoolSetting(Setting {
            data: self.data.map(
                |(newer_value_epoch, newer_value, older_value_opt)| SettingData {
                    newer_value_epoch,
                    newer_value,
                    older_value_opt,
                },
            ),
        })
    }
}

/// What a setting field holds.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Setting(SettingSpec),
    /// A setting followed by a stray byte, which neither side can parse.
    Malformed(SettingSpec),
}

/// One coin type's deny list config and its settings.
struct CoinConfig {
    coin_type: TypeTag,
    config_id: ObjectID,
    pause: Option<Entry>,
    denied: Vec<(SuiAddress, Entry)>,
}

fn setting_field<K>(config_id: ObjectID, key: K, entry: Entry, version: u64) -> Object
where
    K: Serialize + std::fmt::Debug + MoveTypeTagTrait + Clone,
{
    match entry {
        Entry::Setting(spec) => field_object(config_id, key, spec.setting(), version),
        Entry::Malformed(spec) => {
            let good = field_object(config_id, key, spec.setting(), version);
            let m = good.data.try_as_move().unwrap();
            let mut contents = m.contents().to_vec();
            contents.push(0);
            let tag = m.type_().clone().into();
            Object::new_move(
                move_object(tag, version, contents),
                good.owner.clone(),
                digest(),
            )
        }
    }
}

fn deny_list_objects(configs: &[CoinConfig]) -> Vec<Object> {
    let mut objects = vec![];
    // 0x2::deny_list::DenyList { id: UID, lists: Bag }: never read by the check.
    let bag_id = ObjectID::new([0x44; 32]);
    let contents =
        bcs::to_bytes(&(UID::new(SUI_DENY_LIST_OBJECT_ID), UID::new(bag_id), 0u64)).unwrap();
    objects.push(Object::new_move(
        move_object(
            framework_struct("deny_list", "DenyList", vec![]),
            1,
            contents,
        ),
        Owner::Shared {
            initial_shared_version: SequenceNumber::from_u64(1),
        },
        digest(),
    ));
    for c in configs {
        // The config is a dynamic object field of the deny list: a `Field<Wrapper<ConfigKey>, ID>`
        // owned by the deny list, and the `Config<ConfigWriteCap>` owned by that field.
        let key = DOFWrapper {
            name: ConfigKey {
                per_type_index: 0,
                per_type_key: c.coin_type.to_canonical_string(false).into_bytes(),
            },
        };
        let wrapper = field_object(SUI_DENY_LIST_OBJECT_ID, key, ID::new(c.config_id), 5);
        let wrapper_id = wrapper.id();
        objects.push(wrapper);
        let cap = TypeTag::Struct(Box::new(framework_struct(
            "deny_list",
            "ConfigWriteCap",
            vec![],
        )));
        objects.push(Object::new_move(
            move_object(
                framework_struct("config", "Config", vec![cap]),
                5,
                bcs::to_bytes(&UID::new(c.config_id)).unwrap(),
            ),
            Owner::ObjectOwner(wrapper_id.into()),
            digest(),
        ));
        if let Some(entry) = c.pause {
            objects.push(setting_field(c.config_id, GlobalPauseKey(false), entry, 6));
        }
        for (address, entry) in &c.denied {
            objects.push(setting_field(c.config_id, AddressKey(*address), *entry, 7));
        }
    }
    objects
}

/// One store over both object representations.
struct TestStore<'a> {
    reference: StdBTreeMap<ObjectID, Object>,
    ours: StdBTreeMap<messages::base::ObjectId, exec_types::object::Object<'a>>,
}

impl<'a> TestStore<'a> {
    fn new(
        bump: &'a Bump,
        objects: &[Object],
        messages: &'a [Message<messages::object::Object<'static>>],
    ) -> Self {
        let reference = objects.iter().map(|o| (o.id(), o.clone())).collect();
        let ours = messages
            .iter()
            .map(|m| {
                let o = exec_types::object::Object::from_view(bump, m.get());
                (o.id(), o)
            })
            .collect();
        Self { reference, ours }
    }
}

impl sui_types::storage::ObjectStore for TestStore<'_> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        self.reference.get(object_id).cloned()
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: SequenceNumber) -> Option<Object> {
        self.reference
            .get(object_id)
            .filter(|o| o.version() == version)
            .cloned()
    }
}

impl<'a> exec_types::storage::ObjectStore<'a> for TestStore<'a> {
    fn get_object(
        &self,
        object_id: &messages::base::ObjectId,
    ) -> Option<exec_types::object::Object<'a>> {
        self.ours.get(object_id).copied()
    }

    fn get_object_by_key(
        &self,
        object_id: &messages::base::ObjectId,
        version: u64,
    ) -> Option<exec_types::object::Object<'a>> {
        self.ours
            .get(object_id)
            .filter(|o| o.version() == version)
            .copied()
    }
}

/// A result, comparable across the two implementations.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    GlobalPause(String),
    Denied([u8; 32], String),
    Other(String),
}

fn reference_outcome(kind: &RefKind) -> Outcome {
    match kind {
        RefKind::CoinTypeGlobalPause { coin_type } => Outcome::GlobalPause(coin_type.clone()),
        RefKind::AddressDeniedForCoin { address, coin_type } => {
            Outcome::Denied(address.to_inner(), coin_type.clone())
        }
        other => Outcome::Other(format!("{other:?}")),
    }
}

fn our_outcome(kind: &ExecutionErrorKind<'_>) -> Outcome {
    match kind {
        ExecutionErrorKind::CoinTypeGlobalPause { coin_type } => {
            Outcome::GlobalPause(coin_type.to_string())
        }
        ExecutionErrorKind::AddressDeniedForCoin { address, coin_type } => {
            Outcome::Denied(address.0, coin_type.to_string())
        }
        other => Outcome::Other(format!("{other:?}")),
    }
}

/// Runs both checks on `receiving` and asserts they agree; returns the outcome and owner count.
fn run_both(
    configs: &[CoinConfig],
    receiving: &[(TypeTag, Vec<SuiAddress>)],
    cur_epoch: u64,
) -> (Outcome, u64) {
    let objects = deny_list_objects(configs);
    let messages: Vec<_> = objects
        .iter()
        .map(|o| {
            Message::<messages::object::Object<'static>>::parse(bcs::to_bytes(o).unwrap()).unwrap()
        })
        .collect();
    let store_bump = Bump::with_capacity(1 << 12);
    let store = TestStore::new(&store_bump, &objects, &messages);

    let reference_input: StdBTreeMap<TypeTag, StdBTreeSet<SuiAddress>> = receiving
        .iter()
        .map(|(t, owners)| (t.clone(), owners.iter().copied().collect()))
        .collect();
    let reference = sui_types::deny_list_v2::check_coin_deny_list_v2_during_execution(
        reference_input,
        cur_epoch,
        &store,
    );
    let reference_outcome = match &reference.result {
        Ok(()) => Outcome::Ok,
        Err(e) => reference_outcome(e.kind()),
    };

    let bump = Bump::with_capacity(1 << 12);
    let mut ours_input = BTreeMap::new_in(&bump);
    for (t, owners) in receiving {
        let mut set = BTreeSet::new_in(&bump);
        for o in owners {
            set.insert(messages::base::SuiAddress(o.to_inner()));
        }
        ours_input.insert(exec_types::type_tags::type_tag_in(&bump, t), set);
    }
    let ours = executor::deny_list_v2::check_coin_deny_list_v2_during_execution(
        &bump, ours_input, cur_epoch, &store,
    );
    let our_outcome = match &ours.result {
        Ok(()) => Outcome::Ok,
        Err(e) => our_outcome(e.kind()),
    };

    let context = format!("receiving {receiving:?} at epoch {cur_epoch}");
    assert_eq!(our_outcome, reference_outcome, "{context}");
    assert_eq!(
        ours.num_non_gas_coin_owners, reference.num_non_gas_coin_owners,
        "{context}"
    );
    (reference_outcome, reference.num_non_gas_coin_owners)
}

fn coin(address: u8, module: &str, name: &str, type_params: Vec<TypeTag>) -> TypeTag {
    TypeTag::Struct(Box::new(StructTag {
        address: AccountAddress::new([address; 32]),
        module: Identifier::new(module).unwrap(),
        name: Identifier::new(name).unwrap(),
        type_params,
    }))
}

fn sui() -> TypeTag {
    TypeTag::Struct(Box::new(framework_struct("sui", "SUI", vec![])))
}

fn addr(n: u8) -> SuiAddress {
    SuiAddress::from(ObjectID::new([n; 32]))
}

fn regulated() -> TypeTag {
    coin(0xab, "regulated", "REGULATED", vec![])
}

/// A generic coin type, so the canonical string's formatting of type arguments is exercised.
fn generic_regulated() -> TypeTag {
    coin(
        0x0c,
        "pair",
        "LP",
        vec![sui(), TypeTag::U64, TypeTag::Vector(Box::new(TypeTag::U8))],
    )
}

fn plain() -> TypeTag {
    coin(0xcd, "plain", "PLAIN", vec![])
}

fn config(coin_type: TypeTag, id: u8) -> CoinConfig {
    CoinConfig {
        coin_type,
        config_id: ObjectID::new([id; 32]),
        pause: None,
        denied: vec![],
    }
}

#[test]
fn gas_coin_only() {
    let configs = [config(regulated(), 0x51)];
    let (outcome, owners) = run_both(&configs, &[(sui(), vec![addr(1), addr(2)])], 3);
    assert_eq!((outcome, owners), (Outcome::Ok, 0));
    let (outcome, owners) = run_both(&configs, &[], 3);
    assert_eq!((outcome, owners), (Outcome::Ok, 0));
}

#[test]
fn non_regulated_coins() {
    let configs = [config(regulated(), 0x51)];
    let (outcome, owners) = run_both(
        &configs,
        &[
            (sui(), vec![addr(1)]),
            (plain(), vec![addr(1), addr(2), addr(3)]),
            (TypeTag::U64, vec![addr(4)]),
        ],
        3,
    );
    assert_eq!((outcome, owners), (Outcome::Ok, 4));
}

#[test]
fn denied_and_allowed_owners() {
    let mut c = config(regulated(), 0x51);
    c.denied = vec![
        (
            addr(2),
            Entry::Setting(SettingSpec::new(0, Some(true), None)),
        ),
        (
            addr(3),
            Entry::Setting(SettingSpec::new(0, Some(false), None)),
        ),
    ];
    let configs = [c];

    let (outcome, owners) = run_both(&configs, &[(regulated(), vec![addr(1), addr(3)])], 3);
    assert_eq!((outcome, owners), (Outcome::Ok, 2));

    let (outcome, owners) = run_both(
        &configs,
        &[
            (sui(), vec![addr(9)]),
            (regulated(), vec![addr(1), addr(2), addr(3)]),
            (plain(), vec![addr(2)]),
        ],
        3,
    );
    assert_eq!(
        (outcome, owners),
        (
            Outcome::Denied(addr(2).to_inner(), regulated().to_canonical_string(false)),
            4
        )
    );
}

#[test]
fn global_pause() {
    for paused in [false, true] {
        let mut c = config(generic_regulated(), 0x52);
        c.pause = Some(Entry::Setting(SettingSpec::new(1, Some(paused), None)));
        c.denied = vec![(
            addr(2),
            Entry::Setting(SettingSpec::new(1, Some(true), None)),
        )];
        let configs = [c];
        let (outcome, _) = run_both(&configs, &[(generic_regulated(), vec![addr(2)])], 4);
        let coin_type = generic_regulated().to_canonical_string(false);
        let expected = if paused {
            Outcome::GlobalPause(coin_type)
        } else {
            Outcome::Denied(addr(2).to_inner(), coin_type)
        };
        assert_eq!(outcome, expected);
    }
}

/// `newer_value` applies only after `newer_value_epoch`; until then `older_value_opt` does.
#[test]
fn epoch_boundaries() {
    let values = [None, Some(false), Some(true)];
    for newer in values {
        for older in values {
            for cur_epoch in [9, 10, 11] {
                let spec = SettingSpec::new(10, newer, older);
                let mut c = config(regulated(), 0x51);
                c.denied = vec![(addr(2), Entry::Setting(spec))];
                c.pause = Some(Entry::Setting(spec));
                let (outcome, _) = run_both(&[c], &[(regulated(), vec![addr(2)])], cur_epoch);
                let value = if cur_epoch > 10 { newer } else { older };
                let expected = if value == Some(true) {
                    Outcome::GlobalPause(regulated().to_canonical_string(false))
                } else {
                    Outcome::Ok
                };
                assert_eq!(outcome, expected, "{newer:?} {older:?} {cur_epoch}");

                let mut c = config(regulated(), 0x51);
                c.denied = vec![(addr(2), Entry::Setting(spec))];
                let (outcome, _) = run_both(&[c], &[(regulated(), vec![addr(2)])], cur_epoch);
                let expected = if value == Some(true) {
                    Outcome::Denied(addr(2).to_inner(), regulated().to_canonical_string(false))
                } else {
                    Outcome::Ok
                };
                assert_eq!(outcome, expected, "{newer:?} {older:?} {cur_epoch}");
            }
        }
    }
}

/// Settings without data, or that do not parse, read as not set.
#[test]
fn empty_and_malformed_settings() {
    let mut c = config(regulated(), 0x51);
    c.pause = Some(Entry::Malformed(SettingSpec::new(0, Some(true), None)));
    c.denied = vec![
        (addr(1), Entry::Setting(SettingSpec { data: None })),
        (
            addr(2),
            Entry::Malformed(SettingSpec::new(0, Some(true), None)),
        ),
        (
            addr(3),
            Entry::Setting(SettingSpec::new(0, Some(true), None)),
        ),
    ];
    let configs = [c];
    let (outcome, _) = run_both(&configs, &[(regulated(), vec![addr(1), addr(2)])], 5);
    assert_eq!(outcome, Outcome::Ok);
    let (outcome, _) = run_both(
        &configs,
        &[(regulated(), vec![addr(1), addr(2), addr(3)])],
        5,
    );
    assert_eq!(
        outcome,
        Outcome::Denied(addr(3).to_inner(), regulated().to_canonical_string(false))
    );
}

/// Several regulated coin types: the first failure in coin type string order is reported, and
/// every non-gas owner is counted whether or not its coin type is regulated.
#[test]
fn several_coin_types() {
    let mut a = config(regulated(), 0x51);
    a.denied = vec![(
        addr(5),
        Entry::Setting(SettingSpec::new(0, Some(true), None)),
    )];
    let mut b = config(generic_regulated(), 0x52);
    b.pause = Some(Entry::Setting(SettingSpec::new(0, Some(true), None)));
    let configs = [a, b];
    for receiving in [
        vec![
            (regulated(), vec![addr(5)]),
            (generic_regulated(), vec![addr(1)]),
        ],
        vec![
            (regulated(), vec![addr(4)]),
            (generic_regulated(), vec![addr(1)]),
        ],
        vec![
            (regulated(), vec![addr(5), addr(6)]),
            (plain(), vec![addr(5), addr(7)]),
            (sui(), vec![addr(5)]),
        ],
        vec![(plain(), vec![addr(5)]), (TypeTag::Bool, vec![addr(1)])],
    ] {
        let (outcome, _) = run_both(&configs, &receiving, 2);
        assert!(!matches!(outcome, Outcome::Other(_)), "{outcome:?}");
    }
}

/// Pseudo-random state and receiving sets.
#[test]
fn random() {
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: u64| {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        s.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    };
    let types = [regulated(), generic_regulated(), plain(), sui()];
    let bools = [None, Some(false), Some(true)];
    let mut seen = [0; 3];
    for _ in 0..200 {
        let entry = |next: &mut dyn FnMut(u64) -> u64| {
            let spec = if next(8) == 0 {
                SettingSpec { data: None }
            } else {
                SettingSpec::new(next(4), bools[next(3) as usize], bools[next(3) as usize])
            };
            if next(10) == 0 {
                Entry::Malformed(spec)
            } else {
                Entry::Setting(spec)
            }
        };
        let mut configs = vec![];
        for (i, t) in types.iter().take(3).enumerate() {
            if next(4) == 0 {
                continue;
            }
            let mut c = config(t.clone(), 0x51 + i as u8);
            if next(3) == 0 {
                c.pause = Some(entry(&mut next));
            }
            for a in 1..=4 {
                if next(2) == 0 {
                    c.denied.push((addr(a), entry(&mut next)));
                }
            }
            configs.push(c);
        }
        let mut receiving = vec![];
        for t in &types {
            if next(2) == 0 {
                let owners = (1..=4).filter(|_| next(2) == 0).map(addr).collect();
                receiving.push((t.clone(), owners));
            }
        }
        let i = match run_both(&configs, &receiving, next(5)).0 {
            Outcome::Ok => 0,
            Outcome::GlobalPause(_) => 1,
            Outcome::Denied(..) => 2,
            Outcome::Other(o) => panic!("{o}"),
        };
        seen[i] += 1;
    }
    assert!(seen.iter().all(|&n| n > 10), "{seen:?}");
}
