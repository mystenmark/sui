// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::deny_list_v2`: the execution-time coin deny list check. The
//! Move-mirror types (`Config`, `Setting`, `Field`, `DOFWrapper`) and the
//! dynamic field id derivation are sui-types' leaf utilities, read from the
//! objects' contents.

use std::fmt;

use containers::{BTreeMap, BTreeSet, Bump};
use exec_types::base::{EpochId, SUI_DENY_LIST_OBJECT_ID};
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use exec_types::storage::{ObjectStore, SuiError};
use messages::base::{ObjectId, SuiAddress};
use messages::type_tag::TypeTag;
use move_core_types::ident_str;
use move_core_types::language_storage::{StructTag, TypeTag as MoveTypeTag};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sui_types::config::{Config, Setting};
use sui_types::deny_list_v1::{DENY_LIST_COIN_TYPE_INDEX, DENY_LIST_MODULE};
use sui_types::dynamic_field::{DOFWrapper, Field, derive_dynamic_field_id};
use sui_types::{MoveTypeTagTrait, SUI_FRAMEWORK_PACKAGE_ID};

use crate::accumulator_event::is_gas_type;
use crate::storage::DenyListResult;

/// Rust representation of the Move type 0x2::deny_list::ConfigKey.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ConfigKey {
    per_type_index: u64,
    per_type_key: Vec<u8>,
}

impl ConfigKey {
    pub fn type_() -> StructTag {
        StructTag {
            address: SUI_FRAMEWORK_PACKAGE_ID.into(),
            module: DENY_LIST_MODULE.to_owned(),
            name: ident_str!("ConfigKey").to_owned(),
            type_params: vec![],
        }
    }
}

impl MoveTypeTagTrait for ConfigKey {
    fn get_type_tag() -> MoveTypeTag {
        MoveTypeTag::Struct(Box::new(Self::type_()))
    }
}

/// Rust representation of the Move type 0x2::deny_list::AddressKey.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AddressKey(sui_types::base_types::SuiAddress);

impl AddressKey {
    pub fn type_() -> StructTag {
        StructTag {
            address: SUI_FRAMEWORK_PACKAGE_ID.into(),
            module: DENY_LIST_MODULE.to_owned(),
            name: ident_str!("AddressKey").to_owned(),
            type_params: vec![],
        }
    }
}

impl MoveTypeTagTrait for AddressKey {
    fn get_type_tag() -> MoveTypeTag {
        MoveTypeTag::Struct(Box::new(Self::type_()))
    }
}

/// Rust representation of the Move type 0x2::deny_list::GlobalPauseKey.
/// There is no u8 in the Move definition, however empty structs in Move
/// are represented as a single byte 0 in the serialized data.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct GlobalPauseKey(bool);

impl GlobalPauseKey {
    pub fn new() -> Self {
        Self(false)
    }
    pub fn type_() -> StructTag {
        StructTag {
            address: SUI_FRAMEWORK_PACKAGE_ID.into(),
            module: DENY_LIST_MODULE.to_owned(),
            name: ident_str!("GlobalPauseKey").to_owned(),
            type_params: vec![],
        }
    }
}

impl MoveTypeTagTrait for GlobalPauseKey {
    fn get_type_tag() -> MoveTypeTag {
        MoveTypeTag::Struct(Box::new(Self::type_()))
    }
}

/// Returns 1) whether the coin deny list check passed,
///         2) the deny lists checked
///         2) the number of regulated coin owners checked.
pub fn check_coin_deny_list_v2_during_execution<'a>(
    bump: &'a Bump,
    receiving_funds_type_and_owners: BTreeMap<'a, TypeTag<'a>, BTreeSet<'a, SuiAddress>>,
    cur_epoch: EpochId,
    object_store: &dyn ObjectStore<'a>,
) -> DenyListResult<'a> {
    let mut non_gas_coin_owners = BTreeMap::new_in(bump);
    for (ty, owners) in receiving_funds_type_and_owners {
        if !is_gas_type(&ty) {
            non_gas_coin_owners.insert(coin_type_string(bump, &ty), owners);
        }
    }
    let num_non_gas_coin_owners = non_gas_coin_owners.values().map(|v| v.len() as u64).sum();
    let mut regulated_coin_owners = BTreeMap::new_in(bump);
    for (coin_type, owners) in non_gas_coin_owners {
        let Some(deny_list_config) = get_per_type_coin_deny_list_v2(coin_type, object_store) else {
            continue;
        };
        regulated_coin_owners.insert(coin_type, (deny_list_config, owners));
    }
    let result =
        check_new_regulated_coin_owners(bump, regulated_coin_owners, cur_epoch, object_store);
    // `num_non_gas_coin_owners` is used to charge for gas. As such we must be extremely careful
    // to not use a number that is not consistent across all validators. For example, relying on
    // the number of coins with a deny list is _not_ consistent since the deny list is created
    // on the first addition to the deny list. But the total number of coins/owners denied would
    // be consistent since we rely on the results from the last epoch (i.e. relying on the Config's
    // internal invariants)
    DenyListResult {
        result,
        num_non_gas_coin_owners,
    }
}

/// The reference's `ty.to_canonical_string(false)`, in the arena: it derives the per-type config
/// id and reaches effects in error kinds, so it uses the Move `TypeTag`'s formatting.
fn coin_type_string<'a>(bump: &'a Bump, ty: &TypeTag<'_>) -> &'a str {
    let s = exec_types::type_tags::to_move_type_tag(ty).to_canonical_string(false);
    containers::alloc_str(bump, &s)
}

fn check_new_regulated_coin_owners<'a>(
    bump: &'a Bump,
    new_regulated_coin_owners: BTreeMap<'a, &'a str, (Config, BTreeSet<'a, SuiAddress>)>,
    cur_epoch: EpochId,
    object_store: &dyn ObjectStore<'a>,
) -> Result<(), ExecutionError<'a>> {
    for (coin_type, (deny_list, owners)) in new_regulated_coin_owners {
        if check_global_pause(&deny_list, object_store, Some(cur_epoch)) {
            return Err(ExecutionError::new(
                ExecutionErrorKind::CoinTypeGlobalPause { coin_type },
                None,
            ));
        }
        for owner in owners {
            if check_address_denied_by_config(&deny_list, owner, object_store, Some(cur_epoch)) {
                return Err(ExecutionError::new(
                    ExecutionErrorKind::AddressDeniedForCoin {
                        address: containers::alloc(bump, owner),
                        coin_type,
                    },
                    None,
                ));
            }
        }
    }
    Ok(())
}

pub fn get_per_type_coin_deny_list_v2<'a>(
    coin_type: &str,
    object_store: &dyn ObjectStore<'a>,
) -> Option<Config> {
    let config_key = DOFWrapper {
        name: ConfigKey {
            per_type_index: DENY_LIST_COIN_TYPE_INDEX,
            per_type_key: coin_type.as_bytes().to_vec(),
        },
    };
    // TODO: Consider caching the config object UID to avoid repeat deserialization.
    let config: Config =
        get_dynamic_field_from_store(object_store, SUI_DENY_LIST_OBJECT_ID, &config_key).ok()?;
    Some(config)
}

pub fn check_address_denied_by_config<'a>(
    deny_config: &Config,
    address: SuiAddress,
    object_store: &dyn ObjectStore<'a>,
    cur_epoch: Option<EpochId>,
) -> bool {
    let address_key = AddressKey(sui_types::base_types::ObjectID::new(address.0).into());
    read_config_setting(object_store, deny_config, address_key, cur_epoch).unwrap_or(false)
}

pub fn check_global_pause<'a>(
    deny_config: &Config,
    object_store: &dyn ObjectStore<'a>,
    cur_epoch: Option<EpochId>,
) -> bool {
    let global_pause_key = GlobalPauseKey::new();
    read_config_setting(object_store, deny_config, global_pause_key, cur_epoch).unwrap_or(false)
}

/// Fetches the setting from a particular config.
/// Reads the value of the setting, giving `newer_value` if the current epoch is greater than
/// `newer_value_epoch`, and `older_value_opt` otherwise.
/// If `cur_epoch` is `None`, the `newer_value` is always returned.
fn read_config_setting<'a, K, V>(
    object_store: &dyn ObjectStore<'a>,
    config: &Config,
    setting_name: K,
    cur_epoch: Option<EpochId>,
) -> Option<V>
where
    K: Clone + MoveTypeTagTrait + Serialize + DeserializeOwned + fmt::Debug,
    V: Clone + Serialize + DeserializeOwned + fmt::Debug,
{
    let setting: Setting<V> = {
        match get_dynamic_field_from_store(
            object_store,
            ObjectId(config.id.object_id().into_bytes()),
            &setting_name,
        ) {
            Ok(setting) => setting,
            Err(_) => return None,
        }
    };
    setting.read_value(cur_epoch).cloned()
}

/// `sui_types::dynamic_field::get_dynamic_field_from_store`: the value of the field `key` of
/// `parent_id`, read unbounded (the latest version) from `object_store`.
fn get_dynamic_field_from_store<'a, K, V>(
    object_store: &dyn ObjectStore<'a>,
    parent_id: ObjectId,
    key: &K,
) -> Result<V, SuiError>
where
    K: MoveTypeTagTrait + Serialize + DeserializeOwned + fmt::Debug + Clone,
    V: Serialize + DeserializeOwned,
{
    // DynamicFieldKey::object_id
    let id = derive_dynamic_field_id(
        sui_types::base_types::ObjectID::new(parent_id.0),
        &K::get_type_tag(),
        &bcs::to_bytes(key).unwrap(),
    )
    .map_err(|e| SuiError(format!("DynamicFieldReadError: {e}")))?;
    let id = ObjectId(id.into_bytes());
    // UnboundedDynamicFieldID::expect_object
    let object = object_store.get_object(&id).ok_or_else(|| {
        SuiError(format!(
            "DynamicFieldReadError: Dynamic field with key={:?} and ID={:?} not found on parent {:?}",
            key, id, parent_id
        ))
    })?;
    // DynamicFieldObject::load_value
    let move_object = object.try_as_move().ok_or_else(|| {
        SuiError(format!(
            "DynamicFieldReadError: Dynamic field {:?} is not a Move object",
            object.id()
        ))
    })?;
    bcs::from_bytes::<Field<K, V>>(move_object.contents)
        .map(|f| f.value)
        .map_err(|err| SuiError(format!("DynamicFieldReadError: {err}")))
}
