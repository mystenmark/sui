// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod accumulator;
mod fingerprint;
pub(crate) mod object_store;
#[cfg(test)]
mod unit_tests;

use crate::object_runtime::object_store::{CacheMetadata, ChildObjectEffect};

use self::object_store::{ChildObjectEffects, ObjectResult};
use super::get_object_id;
use better_any::{Tid, TidAble};
use containers::{BTreeMap, BTreeSet, Bump, HashMap, IndexMap, IndexSet, Vec};
use exec_types::base::{
    EpochId, SUI_ACCUMULATOR_ROOT_OBJECT_ID, SUI_ADDRESS_ALIAS_STATE_OBJECT_ID,
    SUI_AUTHENTICATOR_STATE_OBJECT_ID, SUI_BRIDGE_OBJECT_ID, SUI_CLOCK_OBJECT_ID,
    SUI_COIN_REGISTRY_OBJECT_ID, SUI_DENY_LIST_OBJECT_ID, SUI_DISPLAY_REGISTRY_OBJECT_ID,
    SUI_FORWARDING_ADDRESS_REGISTRY_OBJECT_ID, SUI_RANDOMNESS_STATE_OBJECT_ID,
    SUI_SYSTEM_STATE_OBJECT_ID,
};
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use exec_types::execution::DynamicallyLoadedObjectMetadata;
use exec_types::object::Object;
use exec_types::storage::{ObjectFundsResolver, ObjectFundsSufficiency, RuntimeObjectResolver};
use messages::base::{ObjectId, SequenceNumber, SuiAddress};
use messages::object::{MoveObjectType, Owner};
use messages::type_tag::{StructTag, TypeTag};
use move_binary_format::errors::{PartialVMError, PartialVMResult};
use move_core_types::{
    account_address::AccountAddress,
    annotated_value::{MoveTypeLayout, MoveValue},
    annotated_visitor as AV, runtime_value as R,
    u256::U256,
    vm_status::StatusCode,
};
use move_vm_runtime::execution::values::{GlobalValue, Value};
use move_vm_runtime::natives::extensions::NativeExtensionMarker;
use object_store::ChildObjectStore;
use std::sync::Arc;
use sui_protocol_config::{LimitThresholdCrossed, ProtocolConfig, check_limit_by_meter};
use sui_types::{error::VMMemoryLimitExceededSubStatusCode, metrics::ExecutionMetrics};
use tracing::error;

pub use accumulator::*;
pub use fingerprint::{SerializedChild, runtime_layouts_equal};

type Set<'a, K> = IndexSet<'a, K>;

#[derive(Debug)]
pub struct LoadedRuntimeObject {
    pub version: SequenceNumber,
    pub is_modified: bool,
}

pub struct RuntimeResults<'a> {
    pub writes: IndexMap<'a, ObjectId, (Owner<'a>, MoveObjectType<'a>, Value)>,
    pub user_events: Vec<'a, (StructTag<'a>, Value)>,
    pub accumulator_events: Vec<'a, MoveAccumulatorEvent<'a>>,
    // Loaded child objects, their loaded version/digest and whether they were modified.
    pub loaded_child_objects: BTreeMap<'a, ObjectId, LoadedRuntimeObject>,
    pub created_object_ids: Set<'a, ObjectId>,
    pub deleted_object_ids: Set<'a, ObjectId>,
    pub settlement_input_sui: u64,
    pub settlement_output_sui: u64,
    /// For child objects in `writes` whose value changed: the value serialized with the layout
    /// the child was loaded with (see `ObjectFingerprint`). It is the bytes of the value in
    /// `writes` as long as that value is not changed.
    pub serialized_children: HashMap<'a, ObjectId, SerializedChild>,
}

#[derive(Clone, Copy)]
struct ObjectFundsAvailable {
    /// Current known available balance.
    /// Keeps a running total available based on sends/redeems in this transaction.
    /// The first time an insufficient balance is reached, we must then query the object store.
    available: U256,
    /// Whether a query to the store has been made.
    queried: bool,
}

pub(crate) struct ObjectRuntimeState<'a> {
    // Only looked up (the reference's is a `BTreeMap`).
    pub(crate) input_objects: HashMap<'a, ObjectId, Owner<'a>>,
    // new ids from object::new. This does not contain any new-and-subsequently-deleted ids
    //
    // The order of `new_ids`, `generated_ids` and `deleted_ids` is never observed: they are only
    // looked up and counted here, and their consumers only look them up or collect them into
    // `BTreeSet`s. So removals swap rather than shift.
    new_ids: Set<'a, ObjectId>,
    // contains all ids generated in the txn including any new-and-subsequently-deleted ids
    generated_ids: Set<'a, ObjectId>,
    // ids passed to object::delete
    deleted_ids: Set<'a, ObjectId>,
    // transfers to a new owner (shared, immutable, object, or account address)
    // TODO these struct tags can be removed if type_to_type_tag was exposed in the session
    transfers: IndexMap<'a, ObjectId, (Owner<'a>, MoveObjectType<'a>, Value)>,
    events: Vec<'a, (StructTag<'a>, Value)>,
    accumulator_events: Vec<'a, MoveAccumulatorEvent<'a>>,
    // total size of events emitted so far
    total_events_size: u64,
    total_events_emitted: u64,
    received: IndexMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    // Used to track SUI conservation in settlement transactions. Settlement transactions
    // gather up withdraws and deposits from other transactions, and record them to accumulator
    // fields. The settlement transaction records the total amount of SUI being disbursed here,
    // so that we can verify that the amount stored in the fields at the end of the transaction
    // is correct.
    settlement_input_sui: u64,
    settlement_output_sui: u64,
    // These three are only looked up (the reference's are `BTreeMap`s).
    accumulator_merge_totals: HashMap<'a, (AccountAddress, TypeTag<'a>), u128>,
    accumulator_split_totals: HashMap<'a, (AccountAddress, TypeTag<'a>), u128>,
    object_funds_available: HashMap<'a, (AccountAddress, TypeTag<'a>), ObjectFundsAvailable>,
}

/// The reference's, without the test scenario's inventories: its natives,
/// for Move unit tests only, are left out.
#[derive(Tid)]
pub struct ObjectRuntime<'a> {
    /// The transaction's arena, which the runtime's state and the types it
    /// records live in.
    pub(crate) bump: &'a Bump,
    child_object_store: ChildObjectStore<'a>,
    object_funds_resolver: &'a dyn ObjectFundsResolver,
    // the internal state
    pub(crate) state: ObjectRuntimeState<'a>,
    // whether or not this TX is gas metered
    is_metered: bool,

    pub(crate) protocol_config: &'a ProtocolConfig,
    pub(crate) metrics: Arc<ExecutionMetrics>,
}

impl<'a> NativeExtensionMarker<'a> for ObjectRuntime<'a> {}

pub enum TransferResult {
    New,
    SameOwner,
    OwnerChanged,
}

pub struct InputObject<'a> {
    /// The reference's is a `BTreeSet`; its ids are only inserted into maps, with the same value
    /// for each, so their order does not matter.
    pub contained_uids: Vec<'a, ObjectId>,
    pub version: SequenceNumber,
    pub owner: Owner<'a>,
}

impl ObjectFundsAvailable {
    /// Initially, the available balance is 0 and no store query has been made.
    fn init() -> Self {
        Self {
            available: U256::from(0u64),
            queried: false,
        }
    }

    fn needs_store_read(&self, amount: U256) -> bool {
        self.available < amount && !self.queried
    }
}

impl<'a> ObjectRuntime<'a> {
    pub fn new(
        bump: &'a Bump,
        object_resolver: &'a dyn RuntimeObjectResolver<'a>,
        object_funds_resolver: &'a dyn ObjectFundsResolver,
        // The reference's is a `BTreeMap`. The order does not matter: no two inputs share an id
        // or a contained UID (a UID is in one object), so each entry below is made once.
        input_objects: Vec<'a, (ObjectId, InputObject<'a>)>,
        is_metered: bool,
        protocol_config: &'a ProtocolConfig,
        metrics: Arc<ExecutionMetrics>,
        epoch_id: EpochId,
    ) -> Self {
        let mut input_object_owners = containers::hash_map(bump, input_objects.len());
        let mut root_version = containers::hash_map(bump, 0);
        let mut wrapped_object_containers = BTreeMap::new_in(bump);
        for (id, input_object) in input_objects {
            let InputObject {
                contained_uids,
                version,
                owner,
            } = input_object;
            let prev = input_object_owners.insert(id, owner);
            debug_assert!(prev.is_none());
            debug_assert!(contained_uids.contains(&id));
            for contained_uid in contained_uids {
                let prev = root_version.insert(contained_uid, version);
                debug_assert!(prev.is_none());
                if contained_uid != id {
                    let prev = wrapped_object_containers.insert(contained_uid, id);
                    debug_assert!(prev.is_none());
                }
            }
        }
        Self {
            bump,
            child_object_store: ChildObjectStore::new(
                bump,
                object_resolver,
                root_version,
                wrapped_object_containers,
                is_metered,
                protocol_config,
                metrics.clone(),
                epoch_id,
            ),
            object_funds_resolver,
            state: ObjectRuntimeState {
                input_objects: input_object_owners,
                new_ids: Set::new_in(bump),
                generated_ids: Set::new_in(bump),
                deleted_ids: Set::new_in(bump),
                transfers: IndexMap::new_in(bump),
                events: Vec::new_in(bump),
                accumulator_events: Vec::new_in(bump),
                total_events_size: 0,
                total_events_emitted: 0,
                received: IndexMap::new_in(bump),
                settlement_input_sui: 0,
                settlement_output_sui: 0,
                accumulator_merge_totals: containers::hash_map(bump, 0),
                accumulator_split_totals: containers::hash_map(bump, 0),
                object_funds_available: containers::hash_map(bump, 0),
            },
            is_metered,
            protocol_config,
            metrics,
        }
    }

    pub fn check_object_funds_sufficiency(
        &mut self,
        owner: SuiAddress,
        type_: &TypeTag<'a>,
        amount: U256,
    ) -> ObjectFundsSufficiency {
        let key = (AccountAddress::new(owner.0), *type_);
        let entry = self
            .state
            .object_funds_available
            .entry(key)
            .or_insert_with(ObjectFundsAvailable::init);
        if entry.needs_store_read(amount) {
            let settled_available = match self
                .object_funds_resolver
                .object_available_balance(owner, type_)
            {
                Ok(balance) => balance,
                Err(e) => {
                    return ObjectFundsSufficiency::LoadError(e.to_string());
                }
            };
            let Some(available) = entry.available.checked_add(U256::from(settled_available)) else {
                return ObjectFundsSufficiency::Overflow;
            };
            entry.available = available;
            entry.queried = true;
        }
        if entry.available >= amount {
            entry.available -= amount;
            ObjectFundsSufficiency::Sufficient
        } else {
            ObjectFundsSufficiency::Insufficient
        }
    }

    pub(crate) fn object_funds_sufficiency_needs_store_read(
        &self,
        owner: SuiAddress,
        type_: &TypeTag<'a>,
        amount: U256,
    ) -> bool {
        self.state
            .object_funds_available
            .get(&(AccountAddress::new(owner.0), *type_))
            .copied()
            .unwrap_or_else(ObjectFundsAvailable::init)
            .needs_store_read(amount)
    }

    pub fn new_id(&mut self, id: ObjectId) -> PartialVMResult<()> {
        // If metered, we use the metered limit (non system tx limit) as the hard limit
        // This macro takes care of that
        if let LimitThresholdCrossed::Hard(_, lim) = check_limit_by_meter!(
            self.is_metered,
            self.state.new_ids.len(),
            self.protocol_config.max_num_new_move_object_ids(),
            self.protocol_config.max_num_new_move_object_ids_system_tx(),
            self.metrics.limits_metrics.excessive_new_move_object_ids
        ) {
            return Err(PartialVMError::new(StatusCode::MEMORY_LIMIT_EXCEEDED)
                .with_message(format!("Creating more than {} IDs is not allowed", lim))
                .with_sub_status(
                    VMMemoryLimitExceededSubStatusCode::NEW_ID_COUNT_LIMIT_EXCEEDED as u64,
                ));
        };

        // remove from deleted_ids for the case in dynamic fields where the Field object was deleted
        // and then re-added in a single transaction. In that case, we also skip adding it
        // to new_ids.
        let was_present = self.state.deleted_ids.swap_remove(&id);
        if !was_present {
            // mark the id as new
            self.state.generated_ids.insert(id);
            self.state.new_ids.insert(id);
        }
        Ok(())
    }

    /// Marks `id` as new via `new_id` and, when `parent` has a tracked root version, records the
    /// same root version for `id`. When `parent` is untracked it must itself be newly created in
    /// this transaction (and transitively to its root), so no root version is recorded.
    pub fn new_id_from_hash(&mut self, parent: ObjectId, id: ObjectId) -> PartialVMResult<()> {
        self.new_id(id)?;
        self.child_object_store
            .inherit_root_version_from_parent(parent, id)?;
        Ok(())
    }

    pub fn delete_id(&mut self, id: ObjectId) -> PartialVMResult<()> {
        // This is defensive because `self.state.deleted_ids` may not indeed
        // be called based on the `was_new` flag
        // Metered transactions don't have limits for now

        if let LimitThresholdCrossed::Hard(_, lim) = check_limit_by_meter!(
            self.is_metered,
            self.state.deleted_ids.len(),
            self.protocol_config.max_num_deleted_move_object_ids(),
            self.protocol_config
                .max_num_deleted_move_object_ids_system_tx(),
            self.metrics
                .limits_metrics
                .excessive_deleted_move_object_ids
        ) {
            return Err(PartialVMError::new(StatusCode::MEMORY_LIMIT_EXCEEDED)
                .with_message(format!("Deleting more than {} IDs is not allowed", lim))
                .with_sub_status(
                    VMMemoryLimitExceededSubStatusCode::DELETED_ID_COUNT_LIMIT_EXCEEDED as u64,
                ));
        };

        let was_new = self.state.new_ids.swap_remove(&id);
        if !was_new {
            self.state.deleted_ids.insert(id);
        }
        Ok(())
    }

    /// In the new PTB adapter, this function is also used for persisting owners at the end
    /// of the transaction. In which case, we don't check the transfer limits.
    pub fn transfer(
        &mut self,
        owner: Owner<'a>,
        ty: MoveObjectType<'a>,
        obj: Value,
        end_of_transaction: bool,
    ) -> PartialVMResult<TransferResult> {
        let id = ObjectId(
            get_object_id(obj.copy_value())?
                .value_as::<AccountAddress>()?
                .into_bytes(),
        );
        // - An object is new if it is contained in the new ids or if it is one of the objects
        //   created during genesis (the system state object or clock).
        // - Otherwise, check the input objects for the previous owner
        // - If it was not in the input objects, it must have been wrapped or must have been a
        //   child object
        let is_framework_obj = [
            SUI_SYSTEM_STATE_OBJECT_ID,
            SUI_CLOCK_OBJECT_ID,
            SUI_AUTHENTICATOR_STATE_OBJECT_ID,
            SUI_RANDOMNESS_STATE_OBJECT_ID,
            SUI_DENY_LIST_OBJECT_ID,
            SUI_BRIDGE_OBJECT_ID,
            SUI_ACCUMULATOR_ROOT_OBJECT_ID,
            SUI_COIN_REGISTRY_OBJECT_ID,
            SUI_DISPLAY_REGISTRY_OBJECT_ID,
            SUI_ADDRESS_ALIAS_STATE_OBJECT_ID,
            SUI_FORWARDING_ADDRESS_REGISTRY_OBJECT_ID,
        ]
        .contains(&id);
        let transfer_result = if self.state.new_ids.contains(&id) {
            TransferResult::New
        } else if let Some(prev_owner) = self.state.input_objects.get(&id) {
            match (&owner, prev_owner) {
                // don't use == for dummy values in Shared, ConsensusAddressOwner, or Party
                (Owner::Shared { .. }, Owner::Shared { .. }) => TransferResult::SameOwner,
                (
                    Owner::ConsensusAddressOwner {
                        owner: new_owner, ..
                    },
                    Owner::ConsensusAddressOwner {
                        owner: old_owner, ..
                    },
                ) if new_owner == old_owner => TransferResult::SameOwner,
                (Owner::Party(new_party), Owner::Party(old_party))
                    if new_party.default_permissions == old_party.default_permissions
                        && new_party.members == old_party.members =>
                {
                    TransferResult::SameOwner
                }
                (new @ Owner::AddressOwner(_), old)
                | (new @ Owner::ObjectOwner(_), old)
                | (new @ Owner::Immutable, old)
                    if new == old =>
                {
                    TransferResult::SameOwner
                }
                _ => TransferResult::OwnerChanged,
            }
        } else if is_framework_obj {
            // framework objects are always created when they are transferred, but the id is
            // hard-coded so it is not yet in new_ids or generated_ids
            self.state.new_ids.insert(id);
            self.state.generated_ids.insert(id);
            TransferResult::New
        } else {
            TransferResult::OwnerChanged
        };
        // assert!(end of transaction ==> same owner)
        if end_of_transaction
            && !matches!(
                transfer_result,
                TransferResult::New | TransferResult::SameOwner
            )
        {
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(
                    format!(
                        "Untransferred object {} had its owner change or was not new",
                        id
                    ),
                ),
            );
        }

        // Metered transactions don't have limits for now

        if let LimitThresholdCrossed::Hard(_, lim) = check_limit_by_meter!(
            // TODO: is this not redundant? Metered TX implies framework obj cannot be transferred
            // We have higher limits for unmetered transactions and framework obj
            // We don't check the limit for objects whose owner is persisted at the end of the
            // transaction
            self.is_metered && !is_framework_obj && !end_of_transaction,
            self.state.transfers.len(),
            self.protocol_config.max_num_transferred_move_object_ids(),
            self.protocol_config
                .max_num_transferred_move_object_ids_system_tx(),
            self.metrics
                .limits_metrics
                .excessive_transferred_move_object_ids
        ) {
            return Err(PartialVMError::new(StatusCode::MEMORY_LIMIT_EXCEEDED)
                .with_message(format!("Transferring more than {} IDs is not allowed", lim))
                .with_sub_status(
                    VMMemoryLimitExceededSubStatusCode::TRANSFER_ID_COUNT_LIMIT_EXCEEDED as u64,
                ));
        };

        self.state.transfers.insert(id, (owner, ty, obj));
        Ok(transfer_result)
    }

    pub fn emit_event(&mut self, tag: StructTag<'a>, event: Value) -> PartialVMResult<()> {
        if self.state.events.len() >= (self.protocol_config.max_num_event_emit() as usize) {
            return Err(max_event_error(self.protocol_config.max_num_event_emit()));
        }
        self.state.events.push((tag, event));
        self.state.total_events_emitted += 1;
        Ok(())
    }

    pub fn take_user_events(&mut self) -> Vec<'a, (StructTag<'a>, Value)> {
        std::mem::replace(&mut self.state.events, Vec::new_in(self.bump))
    }

    // TODO: Eventually we may want to allow larger types for accumulators,
    // and the errors will need to be native error instead of partial VM error.
    pub fn emit_accumulator_event(
        &mut self,
        accumulator_id: ObjectId,
        action: MoveAccumulatorAction,
        target_addr: AccountAddress,
        target_ty: TypeTag<'a>,
        value: MoveAccumulatorValue,
    ) -> PartialVMResult<()> {
        if let MoveAccumulatorValue::U64(amount) = value {
            let key = (target_addr, target_ty);

            match action {
                MoveAccumulatorAction::Merge => {
                    let current = self
                        .state
                        .accumulator_merge_totals
                        .get(&key)
                        .copied()
                        .unwrap_or(0);
                    let new_total = current + amount as u128;
                    if new_total > u64::MAX as u128 {
                        return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                            .with_message(format!(
                                "accumulator merge overflow: total merges {} exceed u64::MAX",
                                new_total
                            )));
                    }
                    self.state.accumulator_merge_totals.insert(key, new_total);
                    if self
                        .protocol_config
                        .check_object_funds_withdraw_in_execution()
                    {
                        let entry = self
                            .state
                            .object_funds_available
                            .entry((target_addr, target_ty))
                            .or_insert_with(ObjectFundsAvailable::init);
                        entry.available = entry
                            .available
                            .checked_add(U256::from(amount as u128))
                            .ok_or_else(|| {
                            PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                                .with_message("object funds available balance overflow".to_string())
                        })?;
                    }
                }
                MoveAccumulatorAction::Split => {
                    let current = self
                        .state
                        .accumulator_split_totals
                        .get(&key)
                        .copied()
                        .unwrap_or(0);
                    let new_total = current + amount as u128;
                    if new_total > u64::MAX as u128 {
                        return Err(PartialVMError::new(StatusCode::ARITHMETIC_ERROR)
                            .with_message(format!(
                                "accumulator split overflow: total splits {} exceed u64::MAX",
                                new_total
                            )));
                    }
                    self.state.accumulator_split_totals.insert(key, new_total);
                }
            }
        }

        let event = MoveAccumulatorEvent {
            accumulator_id,
            action,
            target_addr,
            target_ty,
            value,
        };
        self.state.accumulator_events.push(event);
        Ok(())
    }

    pub(crate) fn child_object_exists(
        &mut self,
        parent: ObjectId,
        child: ObjectId,
    ) -> PartialVMResult<CacheMetadata<bool>> {
        self.child_object_store.object_exists(parent, child)
    }

    pub(crate) fn child_object_exists_and_has_type(
        &mut self,
        parent: ObjectId,
        child: ObjectId,
        child_type: &MoveObjectType<'_>,
    ) -> PartialVMResult<CacheMetadata<bool>> {
        self.child_object_store
            .object_exists_and_has_type(parent, child, child_type)
    }

    pub(super) fn receive_object(
        &mut self,
        parent: ObjectId,
        child: ObjectId,
        child_version: SequenceNumber,
        child_layout: &R::MoveTypeLayout,
        child_fully_annotated_layout: &MoveTypeLayout,
        child_move_type: MoveObjectType<'a>,
    ) -> PartialVMResult<Option<ObjectResult<CacheMetadata<Value>>>> {
        let Some((value, obj_meta)) = self.child_object_store.receive_object(
            parent,
            child,
            child_version,
            child_layout,
            child_fully_annotated_layout,
            child_move_type,
        )?
        else {
            return Ok(None);
        };

        if self
            .protocol_config
            .early_return_receive_object_mismatched_type()
            && let ObjectResult::MismatchedType = &value
            && self.state.received.contains_key(&child)
        {
            // New case due to the new adapter and being able to re-use receiving values at
            // different types
            return Ok(Some(ObjectResult::MismatchedType));
        }

        // NB: It is important that the object only be added to the received set after it has been
        // fully authenticated and loaded.
        if self.state.received.insert(child, obj_meta).is_some() {
            // We should never hit this -- it means that we have received the same object twice which
            // means we have a duplicated a receiving ticket somehow.
            return Err(
                PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR).with_message(format!(
                    "Object {child} at version {child_version} already received. This can only happen \
                    if multiple `Receiving` arguments exist for the same object in the transaction which is impossible."
                )),
            );
        }
        Ok(Some(value))
    }

    /// `child_layout` is taken by value: the child store keeps it to compare the child's final
    /// value with the bytes it was loaded from.
    pub(crate) fn get_or_fetch_child_object(
        &mut self,
        parent: ObjectId,
        child: ObjectId,
        child_layout: R::MoveTypeLayout,
        child_fully_annotated_layout: &MoveTypeLayout,
        child_move_type: MoveObjectType<'a>,
    ) -> PartialVMResult<ObjectResult<CacheMetadata<&mut GlobalValue>>> {
        let res = self.child_object_store.get_or_fetch_object(
            parent,
            child,
            child_layout,
            child_fully_annotated_layout,
            child_move_type,
        )?;
        Ok(match res {
            ObjectResult::MismatchedType => ObjectResult::MismatchedType,
            ObjectResult::Loaded((cache_info, child_object)) => {
                ObjectResult::Loaded((cache_info, &mut child_object.value))
            }
        })
    }

    pub(crate) fn add_child_object(
        &mut self,
        parent: ObjectId,
        child: ObjectId,
        child_move_type: MoveObjectType<'a>,
        child_value: Value,
    ) -> PartialVMResult<()> {
        self.child_object_store
            .add_object(parent, child, child_move_type, child_value)
    }

    pub(crate) fn config_setting_unsequenced_read(
        &mut self,
        config_id: ObjectId,
        name_df_id: ObjectId,
        field_setting_layout: &R::MoveTypeLayout,
        field_setting_object_type: &MoveObjectType<'a>,
    ) -> Option<Value> {
        match self.child_object_store.config_setting_unsequenced_read(
            config_id,
            name_df_id,
            field_setting_layout,
            field_setting_object_type,
        ) {
            Err(e) => {
                error!(
                    "Failed to read config setting.
                    config_id: {config_id},
                    name_df_id: {name_df_id},
                    field_setting_object_type:  {field_setting_object_type:?},
                    error: {e}"
                );
                None
            }
            Ok(ObjectResult::MismatchedType) | Ok(ObjectResult::Loaded(None)) => None,
            Ok(ObjectResult::Loaded(Some(value))) => Some(value),
        }
    }

    /// The package object at exactly `version`.
    pub fn get_package_at_version(
        &self,
        package_id: ObjectId,
        version: SequenceNumber,
    ) -> Option<Object<'a>> {
        self.child_object_store
            .get_package_at_version(package_id, version)
    }

    /// `loaded_runtime_objects` must be what `loaded_runtime_objects()` returns now. The
    /// reference recomputes it here, digests included, and keeps only the versions; the caller
    /// has computed it already, and loading stops before the runtime finishes.
    pub fn finish(
        mut self,
        loaded_runtime_objects: &BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    ) -> Result<RuntimeResults<'a>, ExecutionError<'a>> {
        debug_assert!(
            loaded_runtime_objects
                .iter()
                .map(|(id, m)| (*id, m.version))
                .eq(self.loaded_runtime_object_versions()),
            "the loaded runtime objects given are not the runtime's"
        );
        let loaded_child_objects = loaded_runtime_objects;
        let child_effects = self.child_object_store.take_effects().map_err(|e| {
            ExecutionError::invariant_violation(format!("Failed to take child object effects: {e}"))
        })?;
        self.state
            .finish(self.bump, loaded_child_objects, child_effects)
    }

    pub fn loaded_runtime_objects(
        &self,
    ) -> BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>> {
        // The loaded child objects, and the received objects, should be disjoint. If they are not,
        // this is an error since it could lead to incorrect transaction dependency computations.
        debug_assert!(
            self.child_object_store
                .cached_objects()
                .keys()
                .all(|id| !self.state.received.contains_key(id))
        );
        let mut loaded = BTreeMap::new_in(self.bump);
        loaded.extend(
            self.child_object_store
                .cached_objects()
                .iter()
                .filter_map(|(id, obj_opt)| {
                    obj_opt.as_ref().map(|obj| {
                        (
                            *id,
                            DynamicallyLoadedObjectMetadata {
                                version: obj.version(),
                                digest: obj.digest(),
                                storage_rebate: obj.storage_rebate(),
                                owner: *obj.owner(),
                                previous_transaction: obj.previous_transaction(),
                            },
                        )
                    })
                })
                .chain(self.state.received.iter().map(|(id, meta)| (*id, *meta))),
        );
        loaded
    }

    /// The ids and versions of `loaded_runtime_objects()`, in its order, without the digests.
    fn loaded_runtime_object_versions(&self) -> impl Iterator<Item = (ObjectId, SequenceNumber)> {
        let mut versions = BTreeMap::new_in(self.bump);
        versions.extend(
            self.child_object_store
                .cached_objects()
                .iter()
                .filter_map(|(id, obj_opt)| obj_opt.as_ref().map(|obj| (*id, obj.version())))
                .chain(
                    self.state
                        .received
                        .iter()
                        .map(|(id, meta)| (*id, meta.version)),
                ),
        );
        versions.into_iter()
    }

    /// A map from wrapped objects to the object that wraps them at the beginning of the
    /// transaction.
    pub fn wrapped_object_containers(&self) -> BTreeMap<'a, ObjectId, ObjectId> {
        self.child_object_store.wrapped_object_containers().clone()
    }

    pub fn record_settlement_sui_conservation(&mut self, input_sui: u64, output_sui: u64) {
        self.state.settlement_input_sui += input_sui;
        self.state.settlement_output_sui += output_sui;
    }

    /// Return the set of all object IDs that were created during this transaction, including any
    /// object IDs that were created and then deleted during the transaction.
    pub fn generated_object_ids(&self) -> BTreeSet<'a, ObjectId> {
        let mut ids = BTreeSet::new_in(self.bump);
        ids.extend(self.state.generated_ids.iter().copied());
        ids
    }
}

pub fn max_event_error(max_events: u64) -> PartialVMError {
    PartialVMError::new(StatusCode::MEMORY_LIMIT_EXCEEDED)
        .with_message(format!(
            "Emitting more than {} events is not allowed",
            max_events
        ))
        .with_sub_status(VMMemoryLimitExceededSubStatusCode::EVENT_COUNT_LIMIT_EXCEEDED as u64)
}

impl<'a> ObjectRuntimeState<'a> {
    /// Update `state_view` with the effects of successfully executing a transaction:
    /// - Given the effects of child objects, processes the changes in terms of
    ///   object writes/deletes basedon the previous state and the changes to the child objects.
    /// - Process `transfers` and `input_objects` to determine whether the type of change
    ///   (WriteKind) to the object
    /// - Process `deleted_ids` with previously determined information to determine the
    ///   DeleteKind
    /// - Passes through user events
    pub(crate) fn finish(
        mut self,
        bump: &'a Bump,
        loaded_child_objects: &BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
        child_object_effects: ChildObjectEffects<'a>,
    ) -> Result<RuntimeResults<'a>, ExecutionError<'a>> {
        let mut loaded_child_objects_: BTreeMap<'a, ObjectId, LoadedRuntimeObject> =
            BTreeMap::new_in(bump);
        loaded_child_objects_.extend(loaded_child_objects.iter().map(|(id, metadata)| {
            (
                *id,
                LoadedRuntimeObject {
                    version: metadata.version,
                    is_modified: false,
                },
            )
        }));
        let mut loaded_child_objects = loaded_child_objects_;
        let mut serialized_children = containers::hash_map(bump, 0);
        self.apply_child_object_effects(
            bump,
            &mut loaded_child_objects,
            &mut serialized_children,
            child_object_effects,
        );
        let ObjectRuntimeState {
            input_objects: _,
            new_ids,
            generated_ids,
            deleted_ids,
            transfers,
            events: user_events,
            total_events_size: _,
            received,
            accumulator_events,
            settlement_input_sui,
            settlement_output_sui,
            accumulator_merge_totals: _,
            accumulator_split_totals: _,
            object_funds_available: _,
            total_events_emitted: _,
        } = self;

        // The set of new ids is a subset of the generated ids.
        debug_assert!(new_ids.is_subset(&generated_ids));

        // Check new owners from transfers, reports an error on cycles.
        // TODO can we have cycles in the new system?
        check_circular_ownership(
            bump,
            transfers.iter().map(|(id, (owner, _, _))| (*id, *owner)),
        )?;
        // For both written_objects and deleted_ids, we need to mark the loaded child object as modified.
        // These may not be covered in the child object effects if they are taken out in one PT command and then
        // transferred/deleted in a different command. Marking them as modified will allow us properly determine their
        // mutation category in effects.
        // TODO: This could get error-prone quickly: what if we forgot to mark an object as modified? There may be a cleaner
        // sulution.
        let mut written_objects = IndexMap::with_capacity_in(transfers.len(), bump);
        written_objects.extend(transfers.into_iter().map(|(id, (owner, type_, value))| {
            if let Some(loaded_child) = loaded_child_objects.get_mut(&id) {
                loaded_child.is_modified = true;
            }
            (id, (owner, type_, value))
        }));
        for deleted_id in &deleted_ids {
            if let Some(loaded_child) = loaded_child_objects.get_mut(deleted_id) {
                loaded_child.is_modified = true;
            }
        }

        // Any received objects are viewed as modified. They had to be loaded in order to be
        // received so they must be in the loaded_child_objects map otherwise it's an invariant
        // violation.
        for (received_object, _) in received.into_iter() {
            match loaded_child_objects.get_mut(&received_object) {
                Some(loaded_child) => {
                    loaded_child.is_modified = true;
                }
                None => {
                    return Err(ExecutionError::invariant_violation(format!(
                        "Failed to find received UID {received_object} in loaded child objects."
                    )));
                }
            }
        }

        Ok(RuntimeResults {
            writes: written_objects,
            user_events,
            accumulator_events,
            loaded_child_objects,
            created_object_ids: new_ids,
            deleted_object_ids: deleted_ids,
            settlement_input_sui,
            settlement_output_sui,
            serialized_children,
        })
    }

    pub fn events(&self) -> &[(StructTag<'a>, Value)] {
        &self.events
    }

    pub fn total_events_emitted(&self) -> u64 {
        self.total_events_emitted
    }

    pub fn total_events_size(&self) -> u64 {
        self.total_events_size
    }

    pub fn incr_total_events_size(&mut self, size: u64) {
        self.total_events_size += size;
    }

    fn apply_child_object_effects(
        &mut self,
        bump: &'a Bump,
        loaded_child_objects: &mut BTreeMap<'a, ObjectId, LoadedRuntimeObject>,
        serialized_children: &mut HashMap<'a, ObjectId, SerializedChild>,
        child_object_effects: ChildObjectEffects<'a>,
    ) {
        for (child, child_object_effect) in child_object_effects {
            let ChildObjectEffect {
                owner: parent,
                ty,
                final_value,
                object_changed,
                serialized,
            } = child_object_effect;

            if object_changed {
                if let Some(loaded_child) = loaded_child_objects.get_mut(&child) {
                    loaded_child.is_modified = true;
                }

                match final_value {
                    None => {
                        // Value was changed and is no longer present, it may have been wrapped,
                        // transferred, or deleted.

                        // If it was transferred, it should not have been deleted
                        // transferred ==> !deleted
                        debug_assert!(
                            !self.transfers.contains_key(&child)
                                || !self.deleted_ids.contains(&child)
                        );
                        // If it was deleted, it should not have been transferred. Additionally,
                        // if it was deleted, it should no longer be marked as new.
                        // deleted ==> !transferred and !new
                        debug_assert!(
                            !self.deleted_ids.contains(&child)
                                || (!self.transfers.contains_key(&child)
                                    && !self.new_ids.contains(&child))
                        );
                    }
                    Some(v) => {
                        // Value was changed (or the owner was changed)

                        // It is still a dynamic field so it should not be transferred or deleted
                        debug_assert!(
                            !self.transfers.contains_key(&child)
                                && !self.deleted_ids.contains(&child)
                        );
                        // If it was loaded, it must have been new. But keep in mind if it was not
                        // loaded, it is not necessarily new since it could have been
                        // input/wrapped/received
                        // loaded ==> !new
                        debug_assert!(
                            !loaded_child_objects.contains_key(&child)
                                || !self.new_ids.contains(&child)
                        );
                        // Mark the mutation of the new value and/or parent.
                        let parent = containers::alloc(bump, SuiAddress(parent.0));
                        self.transfers
                            .insert(child, (Owner::ObjectOwner(parent), ty, v));
                        if let Some(serialized) = serialized {
                            serialized_children.insert(child, serialized);
                        }
                    }
                }
            } else {
                // The object was not changed.
                // If it was created,
                //   it must now have been moved elsewhere (wrapped or transferred).
                // If it was deleted or transferred,
                //   it must have been an input/received/wrapped object.
                // In either case, the value must now have been moved elsewhere, giving us:
                // (new or deleted or transferred or received) ==> no value
                // which is equivalent to:
                // has value ==> (!deleted and !transferred and !input)
                // If the value is still there, it must have been loaded.
                // Combining these to give us the check:
                // has value ==> (loaded and !deleted and !transferred and !input and !received)
                // which is equivalent to:
                // !(no value) ==> (loaded and !deleted and !transferred and !input and !received)
                debug_assert!(
                    final_value.is_none()
                        || (loaded_child_objects.contains_key(&child)
                            && !self.deleted_ids.contains(&child)
                            && !self.transfers.contains_key(&child)
                            && !self.input_objects.contains_key(&child)
                            && !self.received.contains_key(&child))
                );
                // In any case, if it was not changed, it should not be marked as modified
                debug_assert!(
                    loaded_child_objects
                        .get(&child)
                        .is_none_or(|loaded_child| !loaded_child.is_modified)
                );
            }
        }
    }
}

fn check_circular_ownership<'a>(
    bump: &'a Bump,
    transfers: impl IntoIterator<Item = (ObjectId, Owner<'a>)>,
) -> Result<(), ExecutionError<'a>> {
    // Only looked up (the reference's is a `BTreeMap`); `transfers` decides the order of checks.
    let mut object_owner_map = containers::hash_map(bump, 0);
    for (id, recipient) in transfers {
        object_owner_map.remove(&id);
        match recipient {
            Owner::AddressOwner(_)
            | Owner::Shared { .. }
            | Owner::Immutable
            | Owner::ConsensusAddressOwner { .. }
            | Owner::Party(_) => (),
            Owner::ObjectOwner(new_owner) => {
                let new_owner = ObjectId(new_owner.0);
                let mut cur = new_owner;
                loop {
                    if cur == id {
                        return Err(ExecutionError::from_kind(
                            ExecutionErrorKind::CircularObjectOwnership {
                                object: containers::alloc(bump, cur),
                            },
                        ));
                    }
                    if let Some(parent) = object_owner_map.get(&cur) {
                        cur = *parent;
                    } else {
                        break;
                    }
                }
                object_owner_map.insert(id, new_owner);
            }
        }
    }
    Ok(())
}

/// WARNING! This function assumes that the bcs bytes have already been validated,
/// and it will give an invariant violation otherwise.
/// In short, we are relying on the invariant that the bytes are valid for objects
/// in storage.  We do not need this invariant for dev-inspect, as the programmable
/// transaction execution will validate the bytes before we get to this point.
pub fn get_all_uids<'a>(
    bump: &'a Bump,
    fully_annotated_layout: &MoveTypeLayout,
    bcs_bytes: &[u8],
) -> Result<Vec<'a, ObjectId>, /* invariant violation */ String> {
    // In the order found, where the reference's is a `BTreeSet`: a valid object holds each UID
    // once, and consumers only insert the ids into maps.
    let mut ids = Vec::new_in(bump);
    struct UIDTraversal<'i, 'a>(&'i mut Vec<'a, ObjectId>);
    struct UIDCollector<'i, 'a>(&'i mut Vec<'a, ObjectId>);

    impl<'b, 'l> AV::Traversal<'b, 'l> for UIDTraversal<'_, '_> {
        type Error = AV::Error;

        fn traverse_struct(
            &mut self,
            driver: &mut AV::StructDriver<'_, 'b, 'l>,
        ) -> Result<(), Self::Error> {
            if is_uid(&driver.struct_layout().type_) {
                while driver.next_field(&mut UIDCollector(self.0))?.is_some() {}
            } else {
                while driver.next_field(self)?.is_some() {}
            }
            Ok(())
        }
    }

    impl<'b, 'l> AV::Traversal<'b, 'l> for UIDCollector<'_, '_> {
        type Error = AV::Error;
        fn traverse_address(
            &mut self,
            _driver: &AV::ValueDriver<'_, 'b, 'l>,
            value: AccountAddress,
        ) -> Result<(), Self::Error> {
            self.0.push(ObjectId(value.into_bytes()));
            Ok(())
        }
    }

    MoveValue::visit_deserialize(
        bcs_bytes,
        fully_annotated_layout,
        &mut UIDTraversal(&mut ids),
    )
    .map_err(|e| format!("Failed to deserialize. {e}"))?;
    Ok(ids)
}

/// `UID::type_()`: `0x2::object::UID`.
fn is_uid(tag: &move_core_types::language_storage::StructTag) -> bool {
    tag.address.into_bytes() == exec_types::base::SUI_FRAMEWORK_ADDRESS.0
        && tag.module.as_str() == "object"
        && tag.name.as_str() == "UID"
        && tag.type_params.is_empty()
}
