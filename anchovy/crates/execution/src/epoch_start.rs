// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What the reference's `EpochStartConfiguration` records of the system
//! objects as an epoch starts, which decides the epoch's last transaction;
//! and the accumulator object count it charges storage for.

use sui_types::accumulator_metadata::get_accumulator_object_count;
use sui_types::base_types::ObjectID;
use sui_types::bridge::is_bridge_committee_initiated;
use sui_types::object::Owner;
use sui_types::storage::ObjectStore as _;

use crate::{Error, Result, StoreView};

/// Each system shared object's initial shared version, if it exists: those
/// of `SYSTEM_SHARED_OBJECT_IDS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemObjects {
    pub authenticator_state: Option<u64>,
    pub randomness_state: Option<u64>,
    pub deny_list: Option<u64>,
    pub bridge: Option<u64>,
    pub accumulator_root: Option<u64>,
    pub coin_registry: Option<u64>,
    pub display_registry: Option<u64>,
    pub address_alias_state: Option<u64>,
    pub forwarding_address_registry: Option<u64>,
    /// Whether the bridge has its committee.
    pub bridge_committee_initiated: bool,
}

/// The system objects as `store` holds them.
pub fn system_objects(store: &store::Store) -> Result<SystemObjects> {
    let view = StoreView::new(store);
    let version = |id: ObjectID| {
        view.get_object(&id).map(|object| match object.owner {
            Owner::Shared {
                initial_shared_version,
            } => initial_shared_version.value(),
            _ => unreachable!("system object {id} is shared"),
        })
    };
    Ok(SystemObjects {
        authenticator_state: version(sui_types::SUI_AUTHENTICATOR_STATE_OBJECT_ID),
        randomness_state: version(sui_types::SUI_RANDOMNESS_STATE_OBJECT_ID),
        deny_list: version(sui_types::SUI_DENY_LIST_OBJECT_ID),
        bridge: version(sui_types::SUI_BRIDGE_OBJECT_ID),
        accumulator_root: version(sui_types::SUI_ACCUMULATOR_ROOT_OBJECT_ID),
        coin_registry: version(sui_types::SUI_COIN_REGISTRY_OBJECT_ID),
        display_registry: version(sui_types::SUI_DISPLAY_REGISTRY_OBJECT_ID),
        address_alias_state: version(sui_types::SUI_ADDRESS_ALIAS_STATE_OBJECT_ID),
        forwarding_address_registry: version(sui_types::SUI_FORWARDING_ADDRESS_REGISTRY_OBJECT_ID),
        bridge_committee_initiated: is_bridge_committee_initiated(&view).map_err(Error::Sui)?,
    })
}

/// How many accumulator objects there are, if recorded.
pub fn accumulator_object_count(store: &store::Store) -> Result<Option<u64>> {
    get_accumulator_object_count(&StoreView::new(store)).map_err(Error::Sui)
}

/// The reference's `EXTRA_FIELD_EXECUTION_TIME_ESTIMATES_CHUNK_COUNT_KEY`
/// (in `sui-core`): the system state's extra field counting the chunks of
/// stored execution time observations.
const EXECUTION_TIME_ESTIMATES_CHUNK_COUNT_KEY: u64 = 1;

/// The execution time observations the previous epoch's last transaction
/// stored in the system state, merged, as the reference's
/// `get_stored_execution_time_observations` reads them for an epoch's
/// estimator: none if `config` stores none, or the store has none.
pub fn stored_execution_time_observations(
    store: &store::Store,
    config: &sui_protocol_config::ProtocolConfig,
) -> messages::build::system_transaction::StoredExecutionTimeObservations {
    use sui_protocol_config::PerObjectCongestionControlMode;
    use sui_types::dynamic_field::get_dynamic_field_from_store;
    use sui_types::execution::ExecutionTimeObservationChunkKey;
    use sui_types::sui_system_state::{SuiSystemState, get_sui_system_state};
    use sui_types::transaction::StoredExecutionTimeObservations;

    let none =
        || messages::build::system_transaction::StoredExecutionTimeObservations::V1(Vec::new());
    let PerObjectCongestionControlMode::ExecutionTimeEstimate(params) =
        config.per_object_congestion_control_mode()
    else {
        return none();
    };
    if params.observations_chunk_size.is_none() {
        return none();
    }
    let view = StoreView::new(store);
    let Ok(SuiSystemState::V2(system_state)) = get_sui_system_state(&view) else {
        return none();
    };
    let fields = system_state.extra_fields.id.id.bytes;
    let Ok::<u64, _>(count) =
        get_dynamic_field_from_store(&view, fields, &EXECUTION_TIME_ESTIMATES_CHUNK_COUNT_KEY)
    else {
        return none();
    };
    let mut chunks = Vec::new();
    for chunk_index in 0..count {
        let key = ExecutionTimeObservationChunkKey { chunk_index };
        let Ok::<Vec<u8>, _>(bytes) = get_dynamic_field_from_store(&view, fields, &key) else {
            return none();
        };
        let chunk: StoredExecutionTimeObservations =
            bcs::from_bytes(&bytes).expect("stored execution time observations deserialize");
        chunks.push(chunk);
    }
    // The same BCS in anchovy's types.
    let merged = bcs::to_bytes(&StoredExecutionTimeObservations::merge_sorted_chunks(
        chunks,
    ))
    .expect("observations serialize");
    bcs::from_bytes(&merged).expect("observations as the reference serializes them")
}
