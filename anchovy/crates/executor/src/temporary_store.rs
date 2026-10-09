// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's `TemporaryStore`, over loaded inputs: what it derived from `InputObjects`
//! and the input object map it cloned are borrowed from `ExecutionInputs`, which nothing changes
//! during execution. Reads that execution discovers (packages, children, received and system
//! objects) go to the backing store.

use crate::accumulator_event::{AccumulatorEvent, signed_balance_changes_from_events};
use crate::accumulator_root;
use crate::deny_list_v2::check_coin_deny_list_v2_during_execution;
use crate::effects::{
    self, EffectsObjectChange, SharedInput, VersionDigest, compute_unchanged_consensus_objects,
    estimate_effects_size_upperbound_v2, merge_accumulator_writes,
};
use crate::error::{ExecutionError, ExecutionErrorKind};
use crate::execution::{ExecutionResultsV2, is_system_package};
use crate::execution_mode::ExecutionMode;
use crate::gas_charger::GasCharger;
use crate::inputs::ExecutionInputs;
use crate::storage::{DenyListResult, Storage, UnsettledObjectFundsRead};
use crate::transaction::{
    get_funds_withdrawals, get_gasless_allowed_token_types, is_gas_paid_from_address_balance,
    is_gasless_transaction,
};
use containers::{BTreeMap, BTreeSet, Bump, Vec};
use exec_types::assert_invariant;
use exec_types::base::{EpochId, SUI_DENY_LIST_OBJECT_ID, SUI_SYSTEM_STATE_OBJECT_ID};
use exec_types::execution::DynamicallyLoadedObjectMetadata;
use exec_types::object::Object;
use exec_types::storage::{
    BackingPackageStore, BackingStore, ObjectFundsResolver, RuntimeObjectResolver, SuiError,
    SuiResult,
};
use messages::base::{ObjectId, SequenceNumber, SuiAddress, TransactionDigest};
use messages::effects::{AccumulatorOperation, AccumulatorValue, Event, GasCostSummary};
use messages::execution_status::ExecutionStatus;
use messages::object::{Data, Owner};
use messages::transaction::{
    Command, GasData, Reservation, TransactionKind, WithdrawFrom, WithdrawalTypeArg,
};
use messages::type_tag::TypeTag;
use move_vm_runtime::runtime::MoveRuntime;
use std::cell::RefCell;
use std::sync::Arc;
use sui_protocol_config::ProtocolConfig;

pub(crate) mod invariants;
use invariants::InvariantChecker;

/// Declared allowance ids per `(funder, funds type)` key.
type AllowanceIds<'a> = BTreeMap<'a, (SuiAddress, TypeTag<'a>), Vec<'a, ObjectId>>;

struct PostExecutionCheckInputs<'a> {
    /// Per-`(address, type)` funds-accumulator reservation budget authorized by this transaction.
    /// Shared by gasless execution validation and the post-execution invariant checks.
    input_reservations: BTreeMap<'a, (SuiAddress, TypeTag<'a>), u64>,
    /// The allowance ids declared per `WithdrawFrom::SenderAllowance` reservation key. Consumed by
    /// `check_ownership_invariants` to authorize Splits at non-signer keys.
    allowance_ids: AllowanceIds<'a>,
    /// For the advance-epoch transaction, `(epoch_fees minted, epoch_rebates burned)`; `None`
    /// for every other transaction. Needed by the expensive SUI conservation check.
    advance_epoch_gas_summary: Option<(u64, u64)>,
    /// The genesis transaction mints the initial SUI supply and so is exempt from conservation.
    is_genesis: bool,
    /// What each `Publish`/`Upgrade` command in the PTB says the package it writes should look like.
    /// `None` when the transaction is not a PTB.
    declared_packages: Option<Vec<'a, (usize, BTreeSet<'a, ObjectId>)>>,
}

impl<'a> PostExecutionCheckInputs<'a> {
    fn new(
        bump: &'a Bump,
        transaction: (&TransactionKind<'a>, &GasData<'a>, SuiAddress),
        enable_gasless: bool,
    ) -> Self {
        let (transaction_kind, gas_data, transaction_signer) = transaction;
        let (input_reservations, allowance_ids) = compute_input_reservations(
            bump,
            transaction_kind,
            gas_data,
            transaction_signer,
            enable_gasless,
        );
        Self {
            input_reservations,
            allowance_ids,
            advance_epoch_gas_summary: get_advance_epoch_tx_gas_summary(transaction_kind),
            is_genesis: matches!(transaction_kind, TransactionKind::Genesis(_)),
            declared_packages: declared_packages(bump, transaction_kind),
        }
    }

    fn genesis(bump: &'a Bump) -> Self {
        Self {
            input_reservations: BTreeMap::new_in(bump),
            allowance_ids: BTreeMap::new_in(bump),
            advance_epoch_gas_summary: None,
            is_genesis: true,
            declared_packages: None,
        }
    }
}

pub struct TemporaryStore<'a> {
    bump: &'a Bump,
    // The backing store for retrieving Move packages onchain.
    // When executing a Move call, the dependent packages are not going to be
    // in the input objects. They will be fetched from the backing store.
    store: &'a dyn BackingStore<'a>,
    tx_digest: TransactionDigest,
    /// The loaded inputs, from which the reference derives its input object map, mutable input
    /// refs, non-exclusive originals, stream-ended objects, receiving objects and lamport
    /// timestamp.
    inputs: &'a ExecutionInputs<'a>,
    /// Immutable transaction-derived inputs needed for various checks after execution finishes.
    post_execution_check_inputs: PostExecutionCheckInputs<'a>,
    execution_results: ExecutionResultsV2<'a>,
    /// Objects that were loaded during execution (dynamic fields + received objects).
    loaded_runtime_objects: BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    protocol_config: &'a ProtocolConfig,
    // The reference also keeps every package read from the store
    // (`runtime_packages_loaded_from_db`), for sui-core's fork debugging; nothing here reads it.
    /// The current epoch.
    cur_epoch: EpochId,

    /// The set of per-epoch config objects that were loaded during execution, and are not in the
    /// input objects. This allows us to commit them to the effects.
    // The reference's `RwLock`: the store is used from one thread.
    loaded_per_epoch_config_objects: RefCell<BTreeSet<'a, ObjectId>>,

    /// Execution-attempt bookkeeping for post-execution system checks.
    invariants: InvariantChecker<'a>,

    /// System objects implicitly read during execution, keyed by object ID, with the version (and its
    /// digest) at which they were read.
    /// Interior-mutable because reads happen behind `&self` (`RuntimeObjectResolver`).
    loaded_system_objects: RefCell<BTreeMap<'a, ObjectId, VersionDigest>>,

    unsettled_object_funds: &'a dyn UnsettledObjectFundsRead,
}

/// `sui_types::inner_temporary_store::InnerTemporaryStore`, without what it repeats of the
/// inputs (the caller has them).
pub struct InnerTemporaryStore<'a> {
    pub written: BTreeMap<'a, ObjectId, Object<'a>>,
    pub events: Vec<'a, Event<'a>>,
    pub accumulator_events: Vec<'a, AccumulatorEvent<'a>>,
    pub loaded_runtime_objects: BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    pub lamport_version: SequenceNumber,
    pub accumulator_running_max_withdraws: BTreeMap<'a, ObjectId, u128>,
    /// Not in the reference, which hands its caller the events to encode again: the encoded
    /// events whose digest the effects carry, if there are any.
    pub encoded_events: Option<messages::fast::Built<'a>>,
    /// Not in the reference, whose caller reads them back from the effects: the objects left
    /// without a live version, as the effects' `deleted`, `wrapped` and
    /// `unwrapped_then_deleted` classify them.
    pub removed: Vec<'a, ObjectId>,
}

impl<'a> TemporaryStore<'a> {
    /// Creates a new store associated with an authority store, and populates it with
    /// initial objects.
    pub fn new(
        bump: &'a Bump,
        store: &'a dyn BackingStore<'a>,
        inputs: &'a ExecutionInputs<'a>,
        tx_digest: TransactionDigest,
        protocol_config: &'a ProtocolConfig,
        cur_epoch: EpochId,
        transaction: (&TransactionKind<'a>, &GasData<'a>, SuiAddress),
        unsettled_object_funds: &'a dyn UnsettledObjectFundsRead,
    ) -> Self {
        let post_execution_check_inputs =
            PostExecutionCheckInputs::new(bump, transaction, protocol_config.enable_gasless());
        Self::new_with_input_objects(
            bump,
            store,
            inputs,
            tx_digest,
            protocol_config,
            cur_epoch,
            post_execution_check_inputs,
            unsettled_object_funds,
        )
    }

    pub fn new_for_genesis_state_update(
        bump: &'a Bump,
        store: &'a dyn BackingStore<'a>,
        inputs: &'a ExecutionInputs<'a>,
        tx_digest: TransactionDigest,
        protocol_config: &'a ProtocolConfig,
    ) -> Self {
        // The genesis transaction cannot withdraw object funds, so there are never
        // unsettled withdrawals for it to account for.
        Self::new_with_input_objects(
            bump,
            store,
            inputs,
            tx_digest,
            protocol_config,
            0,
            PostExecutionCheckInputs::genesis(bump),
            &crate::storage::EmptyUnsettledObjectFunds,
        )
    }

    fn new_with_input_objects(
        bump: &'a Bump,
        store: &'a dyn BackingStore<'a>,
        inputs: &'a ExecutionInputs<'a>,
        tx_digest: TransactionDigest,
        protocol_config: &'a ProtocolConfig,
        cur_epoch: EpochId,
        post_execution_check_inputs: PostExecutionCheckInputs<'a>,
        unsettled_object_funds: &'a dyn UnsettledObjectFundsRead,
    ) -> Self {
        #[cfg(debug_assertions)]
        {
            // Ensure that input objects and receiving objects must not overlap.
            assert!(
                inputs
                    .receiving_objects()
                    .iter()
                    .all(|oref| !inputs.objects().contains_key(&oref.0))
            );
        }
        Self {
            bump,
            store,
            tx_digest,
            inputs,
            execution_results: ExecutionResultsV2::new_in(bump),
            protocol_config,
            loaded_runtime_objects: BTreeMap::new_in(bump),
            cur_epoch,
            loaded_per_epoch_config_objects: RefCell::new(BTreeSet::new_in(bump)),
            post_execution_check_inputs,
            invariants: InvariantChecker::new_in(bump),
            loaded_system_objects: RefCell::new(BTreeMap::new_in(bump)),
            unsettled_object_funds,
        }
    }

    /// Checks that the system object `object_id` is available at the version this transaction
    /// requires, and records the read so it can be emitted into effects
    /// and reproduced on replay.
    /// This is expected to return Some in normal cases. If it ever returns None, it should be
    /// treated as an invariant violation.
    pub fn load_implicitly_read_system_object(&self, object_id: &ObjectId) -> Option<Object<'a>> {
        let Some(version) = self.inputs.system_object_version(object_id) else {
            // The reference reports this through `debug_fatal!`.
            debug_assert!(
                false,
                "system_object_versions must contain entry for object_id: {object_id:?}"
            );
            return None;
        };
        let object = self
            .store
            // If this transaction needs to read an implicit system object,
            // the version must be assigned before execution.
            .load_implicitly_read_system_object(object_id, version)?;
        // Record the read version so it can be emitted into effects as a read-only consensus object and
        // reproduced on replay.
        self.loaded_system_objects
            .borrow_mut()
            .insert(*object_id, (object.version(), object.digest()));
        Some(object)
    }

    /// The transaction's arena.
    pub fn bump(&self) -> &'a Bump {
        self.bump
    }

    pub fn unsettled_object_funds(&self) -> &dyn UnsettledObjectFundsRead {
        self.unsettled_object_funds
    }

    // Helpers to access private fields
    pub fn objects(&self) -> &BTreeMap<'a, ObjectId, Object<'a>> {
        self.inputs.objects()
    }

    pub fn update_object_version_and_prev_tx(&mut self) {
        self.execution_results.update_version_and_previous_tx(
            self.inputs.lamport_timestamp(),
            self.tx_digest,
            self.inputs.objects(),
            self.protocol_config.reshare_at_same_initial_version(),
        );

        #[cfg(debug_assertions)]
        {
            self.check_invariants();
        }
    }

    fn calculate_accumulator_running_max_withdraws(&self) -> BTreeMap<'a, ObjectId, u128> {
        let mut running_net_withdraws: BTreeMap<ObjectId, i128> = BTreeMap::new_in(self.bump);
        let mut running_max_withdraws: BTreeMap<ObjectId, u128> = BTreeMap::new_in(self.bump);
        for event in &self.execution_results.accumulator_events {
            match &event.write.value {
                AccumulatorValue::Integer(amount) => match event.write.operation {
                    AccumulatorOperation::Split => {
                        let entry = running_net_withdraws
                            .entry(event.accumulator_obj)
                            .or_default();
                        *entry += *amount as i128;
                        if *entry > 0 {
                            let max_entry = running_max_withdraws
                                .entry(event.accumulator_obj)
                                .or_default();
                            *max_entry = (*max_entry).max(*entry as u128);
                        }
                    }
                    AccumulatorOperation::Merge => {
                        let entry = running_net_withdraws
                            .entry(event.accumulator_obj)
                            .or_default();
                        *entry -= *amount as i128;
                    }
                },
                AccumulatorValue::IntegerTuple(_, _) | AccumulatorValue::EventDigest(_) => {}
            }
        }
        running_max_withdraws
    }

    /// Ensure that, per accumulator object, the gross Merge total and gross Split total are
    /// representable: bounded by the total SUI supply for `Balance<SUI>` keys, and by `u64::MAX`
    /// otherwise.
    ///
    /// `AccumulatorWriteV1::merge` folds all writes for a key by summing Merge amounts and Split
    /// amounts separately into `u64`s. The object runtime caps Move-native merges per key at
    /// `u64::MAX`, but the gas charger emits additional, uncapped SUI deposit/withdraw events during
    /// gas smashing and gas charging (e.g. a refund Merge to an address balance), so a per-key SUI
    /// total could be pushed past `u64::MAX`, overflowing that fold (and the SUI-conservation sum).
    /// Reaching such a total requires SUI from an object-sourced withdrawal whose backing is only
    /// verified at settlement.
    ///
    /// Bounding SUI to `TOTAL_SUPPLY_MIST` rejects any such amount here, *before* gas is charged, so
    /// the rejected PTB-emitted writes are dropped on gas reset and only the (bounded) gas events
    /// remain. Crucially, `TOTAL_SUPPLY_MIST` is ~8.4B SUI below `u64::MAX`, so the gas events emitted
    /// after this check (which move only real SUI) cannot push any per-key total past `u64::MAX` -
    /// hence they need not be re-checked. Non-SUI balances have no uncapped gas path, so the
    /// object-runtime per-key `u64::MAX` cap is the binding guard there and we only backstop u64
    /// representability.
    ///
    /// The per-key limits are not sufficient on their own: withdrawn SUI can be spread across several
    /// object keys (each withdrawal `<= TOTAL_SUPPLY_MIST`) and then recombined *outside* the
    /// accumulator - e.g. each withdrawal redeemed to a `Coin<SUI>` and merged into the PTB gas coin
    /// via `MergeCoins`, which is an object mutation, not an accumulator event. The recombined coin
    /// can then reach `u64::MAX` and overflow `deduct_gas` on a refund. So we also bound the
    /// *cross-key* total SUI withdrawn (gross Split) to the supply, capping the total SUI a single
    /// transaction can withdraw regardless of how it is later recombined.
    pub(crate) fn check_accumulator_amounts_representable(&self) -> Result<(), ExecutionError<'a>> {
        let supply = sui_types::gas_coin::TOTAL_SUPPLY_MIST as u128;
        let mut merge_totals: BTreeMap<ObjectId, u128> = BTreeMap::new_in(self.bump);
        let mut split_totals: BTreeMap<ObjectId, u128> = BTreeMap::new_in(self.bump);
        // Cross-key total of SUI withdrawn (gross Split), bounded to the supply (see above).
        let mut total_sui_split: u128 = 0;
        for event in &self.execution_results.accumulator_events {
            let AccumulatorValue::Integer(amount) = event.write.value else {
                continue;
            };
            let amount = amount as u128;
            // SUI cannot exceed its total supply through any single balance. Bounding to the supply
            // (rather than u64::MAX) leaves headroom for the not-yet-emitted gas events.
            let is_sui = matches!(&event.write.ty, TypeTag::Struct(s) if crate::accumulator_event::is_gas_balance(s));
            let limit = if is_sui { supply } else { u64::MAX as u128 };
            let total = match event.write.operation {
                AccumulatorOperation::Merge => {
                    merge_totals.entry(event.accumulator_obj).or_default()
                }
                AccumulatorOperation::Split => {
                    split_totals.entry(event.accumulator_obj).or_default()
                }
            };
            *total += amount;
            if *total > limit {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::CoinBalanceOverflow,
                    format!(
                        "accumulator balance change for {:?} exceeds the representable limit \
                         (gross total {}, limit {})",
                        event.accumulator_obj, *total, limit
                    ),
                ));
            }
            if is_sui && matches!(event.write.operation, AccumulatorOperation::Split) {
                total_sui_split += amount;
                if total_sui_split > supply {
                    return Err(ExecutionError::new_with_source(
                        ExecutionErrorKind::CoinBalanceOverflow,
                        format!(
                            "total SUI withdrawn across all accumulators ({total_sui_split}) \
                             exceeds the total supply ({supply})"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Ensure that there is one entry for each accumulator object in the accumulator events.
    fn merge_accumulator_events(&mut self) {
        let mut by_object: BTreeMap<ObjectId, Vec<_>> = BTreeMap::new_in(self.bump);
        for event in &self.execution_results.accumulator_events {
            by_object
                .entry(event.accumulator_obj)
                .or_insert_with(|| Vec::new_in(self.bump))
                .push(event.write);
        }
        let mut merged = Vec::with_capacity_in(by_object.len(), self.bump);
        for (obj_id, writes) in by_object {
            merged.push(AccumulatorEvent::new(
                obj_id,
                merge_accumulator_writes(self.bump, &writes),
            ));
        }
        self.execution_results.accumulator_events = merged;
    }

    /// Break up the structure and return its internal stores (objects, active_inputs, written, deleted)
    pub fn into_inner(
        self,
        accumulator_running_max_withdraws: BTreeMap<'a, ObjectId, u128>,
    ) -> InnerTemporaryStore<'a> {
        let results = self.execution_results;
        InnerTemporaryStore {
            written: results.written_objects,
            events: results.user_events,
            accumulator_events: results.accumulator_events,
            loaded_runtime_objects: self.loaded_runtime_objects,
            lamport_version: self.inputs.lamport_timestamp(),
            accumulator_running_max_withdraws,
            encoded_events: None,
            removed: Vec::new_in(self.bump),
        }
    }

    /// For every object from active_inputs (i.e. all mutable objects), if they are not
    /// mutated during the transaction execution, force mutating them by incrementing the
    /// sequence number. This is required to achieve safety.
    pub(crate) fn ensure_active_inputs_mutated(&mut self) {
        // The reference gathers copies first because it iterates `self` while mutating it; the
        // inputs are borrowed apart from `self` here.
        let inputs = self.inputs;
        // Note: we do not mutate input objects if they are non-exclusive write
        for id in inputs.exclusive_mutable_inputs().keys() {
            if !self.execution_results.modified_objects.contains(id) {
                // The object must be mutated as it was present in the input objects
                self.mutate_input_object(inputs.objects()[id]);
            }
        }
    }

    fn get_object_changes(&self) -> BTreeMap<'a, ObjectId, EffectsObjectChange<'a>> {
        let results = &self.execution_results;
        let mut all_ids = BTreeSet::new_in(self.bump);
        all_ids.extend(
            results
                .created_object_ids
                .iter()
                .chain(&results.deleted_object_ids)
                .chain(&results.modified_objects)
                .chain(results.written_objects.keys()),
        );
        let mut changes = BTreeMap::new_in(self.bump);
        for id in all_ids {
            changes.insert(
                *id,
                EffectsObjectChange::new(
                    self.bump,
                    self.get_object_modified_at(id)
                        .map(|metadata| ((metadata.version, metadata.digest), metadata.owner)),
                    results.written_objects.get(id),
                    results.created_object_ids.contains(id),
                    results.deleted_object_ids.contains(id),
                ),
            );
        }
        for AccumulatorEvent {
            accumulator_obj,
            write,
        } in &results.accumulator_events
        {
            changes.insert(
                *accumulator_obj,
                EffectsObjectChange::new_from_accumulator_write(self.bump, *write),
            );
        }
        changes
    }

    /// Returns the inner store and the effects, built as `messages::fast` bytes.
    pub fn into_effects(
        mut self,
        shared_object_refs: &[SharedInput],
        transaction_digest: &TransactionDigest,
        mut transaction_dependencies: BTreeSet<'a, TransactionDigest>,
        gas_cost_summary: GasCostSummary,
        status: ExecutionStatus<'a>,
        gas_coin: Option<ObjectId>,
        epoch: EpochId,
    ) -> (InnerTemporaryStore<'a>, messages::fast::Built<'a>) {
        // Defense-in-depth: Owner::Party is not yet supported as an effect output. There are
        // no constructions of `Owner::Party` yet so a hard assert should be safe.
        for (id, obj) in &self.execution_results.written_objects {
            assert!(
                !matches!(obj.owner(), Owner::Party(_)),
                "Party-owned objects are not yet supported (object {id})"
            );
        }

        self.update_object_version_and_prev_tx();
        // The reference's objects compute their digests on demand; these encode once here, the
        // form they are digested and stored in.
        for object in self.execution_results.written_objects.values_mut() {
            *object = object.seal(self.bump);
        }
        // This must happens before merge_accumulator_events.
        let accumulator_running_max_withdraws = self.calculate_accumulator_running_max_withdraws();
        self.merge_accumulator_events();

        if !self.protocol_config.disable_effects_tx_dependencies() {
            // Even on abort, successfully receiving an object creates a dependency.
            for (id, expected_version, expected_digest) in self.inputs.receiving_objects() {
                if let Some(obj_meta) = self.loaded_runtime_objects.get(id) {
                    // A dynamic-field load can spoof a receiving input, so authenticate the
                    // version, digest and owner before registering its dependency.
                    let loaded_via_receive = obj_meta.version == *expected_version
                        && obj_meta.digest == *expected_digest
                        && matches!(obj_meta.owner, Owner::AddressOwner(_));
                    if loaded_via_receive {
                        transaction_dependencies.insert(obj_meta.previous_transaction);
                    }
                }
            }
        }

        assert!(self.protocol_config.enable_effects_v2());

        let object_changes = self.get_object_changes();

        let lamport_version = self.inputs.lamport_timestamp();
        // The reference clones these sets; they are only borrowed here.
        let loaded_per_epoch_config_objects = self.loaded_per_epoch_config_objects.borrow();
        let loaded_system_objects = self.loaded_system_objects.borrow();
        let unchanged_consensus_objects = compute_unchanged_consensus_objects(
            self.bump,
            shared_object_refs,
            &loaded_per_epoch_config_objects,
            &object_changes,
            &loaded_system_objects,
        );
        drop(loaded_per_epoch_config_objects);
        drop(loaded_system_objects);
        let bump = self.bump;
        let mut inner = self.into_inner(accumulator_running_max_withdraws);
        inner.removed.extend(
            object_changes
                .iter()
                .filter(|(_, change)| removes_live_version(change))
                .map(|(id, _)| *id),
        );

        inner.encoded_events =
            (!inner.events.is_empty()).then(|| effects::build_events(bump, &inner.events));
        let events_digest = inner.encoded_events.as_ref().map(|built| built.digest);
        let effects = effects::new_from_execution_v2(
            bump,
            status,
            epoch,
            gas_cost_summary,
            unchanged_consensus_objects,
            *transaction_digest,
            lamport_version,
            object_changes,
            gas_coin,
            events_digest,
            transaction_dependencies,
        );

        (inner, effects)
    }

    /// An internal check of the invariants (will only fire in debug)
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Check not both deleted and written
        debug_assert!(
            {
                self.execution_results
                    .written_objects
                    .keys()
                    .all(|id| !self.execution_results.deleted_object_ids.contains(id))
            },
            "Object both written and deleted."
        );

        // Check all mutable inputs are modified
        debug_assert!(
            {
                self.inputs
                    .exclusive_mutable_inputs()
                    .keys()
                    .all(|id| self.execution_results.modified_objects.contains(id))
            },
            "Mutable input not modified."
        );

        debug_assert!(
            {
                self.execution_results
                    .written_objects
                    .values()
                    .all(|obj| obj.previous_transaction() == self.tx_digest)
            },
            "Object previous transaction not properly set",
        );
    }

    /// Mutate a mutable input object. This is used to mutate input objects outside of PT execution.
    pub fn mutate_input_object(&mut self, object: Object<'a>) {
        let id = object.id();
        debug_assert!(self.inputs.objects().contains_key(&id));
        debug_assert!(!object.is_immutable());
        self.execution_results.modified_objects.insert(id);
        self.execution_results.written_objects.insert(id, object);
    }

    pub fn mutate_new_or_input_object(&mut self, object: Object<'a>) {
        let id = object.id();
        debug_assert!(!object.is_immutable());
        if self.inputs.objects().contains_key(&id) {
            self.execution_results.modified_objects.insert(id);
        }
        self.execution_results.written_objects.insert(id, object);
    }

    /// Mutate a child object outside of PT. This should be used extremely rarely.
    /// Currently it's only used by advance_epoch_safe_mode because it's all native
    /// without PT. This should almost never be used otherwise.
    pub fn mutate_child_object(&mut self, old_object: Object<'a>, new_object: Object<'a>) {
        let id = new_object.id();
        let old_ref = old_object.compute_object_reference();
        debug_assert_eq!(old_ref.0, id);
        self.loaded_runtime_objects.insert(
            id,
            DynamicallyLoadedObjectMetadata {
                version: old_ref.1,
                digest: old_ref.2,
                owner: *old_object.owner(),
                storage_rebate: old_object.storage_rebate(),
                previous_transaction: old_object.previous_transaction(),
            },
        );
        self.execution_results.modified_objects.insert(id);
        self.execution_results
            .written_objects
            .insert(id, new_object);
    }

    /// Upgrade system package during epoch change. This requires special treatment
    /// since the system package to be upgraded is not in the input objects.
    /// We could probably fix above to make it less special.
    pub fn upgrade_system_package(&mut self, package: Object<'a>) {
        let id = package.id();
        assert!(package.is_package() && is_system_package(&id));
        self.execution_results.modified_objects.insert(id);
        self.execution_results.written_objects.insert(id, package);
    }

    /// Crate a new objcet. This is used to create objects outside of PT execution.
    pub fn create_object(&mut self, object: Object<'a>) {
        // Created mutable objects' versions are set to the store's lamport timestamp when it is
        // committed to effects. Creating an object at a non-zero version risks violating the
        // lamport timestamp invariant (that a transaction's lamport timestamp is strictly greater
        // than all versions witnessed by the transaction).
        debug_assert!(
            object.is_immutable() || object.version() == 0,
            "Created mutable objects should not have a version set",
        );
        let id = object.id();
        self.execution_results.created_object_ids.insert(id);
        self.execution_results.written_objects.insert(id, object);
    }

    /// Delete a mutable input object. This is used to delete input objects outside of PT execution.
    pub fn delete_input_object(&mut self, id: &ObjectId) {
        // there should be no deletion after write
        debug_assert!(!self.execution_results.written_objects.contains_key(id));
        debug_assert!(self.inputs.objects().contains_key(id));
        self.execution_results.modified_objects.insert(*id);
        self.execution_results.deleted_object_ids.insert(*id);
    }

    pub fn drop_writes(&mut self) {
        self.execution_results.drop_writes();
        self.invariants = InvariantChecker::new_in(self.bump);
    }

    /// Consume this (post-execution) store and return the store used by a `BumpOnly` exit: keep
    /// the input-derived state as well as information about the execution needed for replay,
    /// discard everything related to the execution results, then bump the mutable
    /// inputs. Dependencies are retained only when enabled by the protocol.
    pub(crate) fn into_bump_only(self) -> Self {
        let Self {
            bump,
            // Input-derived - reused verbatim.
            store,
            tx_digest,
            inputs,
            cur_epoch,
            protocol_config,
            post_execution_check_inputs,
            // Represents what happened during execution, which needs to be kept.
            loaded_runtime_objects,
            loaded_per_epoch_config_objects,
            loaded_system_objects,
            unsettled_object_funds,
            // Execution outcomes can be discarded.
            execution_results: _,
            invariants: _,
        } = self;
        let mut bump_only = Self {
            bump,
            store,
            tx_digest,
            inputs,
            cur_epoch,
            protocol_config,
            loaded_runtime_objects,
            loaded_per_epoch_config_objects,
            post_execution_check_inputs,
            loaded_system_objects,
            unsettled_object_funds,
            execution_results: ExecutionResultsV2::new_in(bump),
            invariants: InvariantChecker::new_in(bump),
        };
        // The only writes a BumpOnly exit records: bump the versions of the mutable inputs it locked.
        bump_only.ensure_active_inputs_mutated();
        bump_only
    }

    pub fn read_object(&self, id: &ObjectId) -> Option<&Object<'a>> {
        // there should be no read after delete
        debug_assert!(!self.execution_results.deleted_object_ids.contains(id));
        self.execution_results
            .written_objects
            .get(id)
            .or_else(|| self.inputs.objects().get(id))
    }

    pub fn save_loaded_runtime_objects(
        &mut self,
        loaded_runtime_objects: BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    ) {
        #[cfg(debug_assertions)]
        {
            for (id, v1) in &loaded_runtime_objects {
                if let Some(v2) = self.loaded_runtime_objects.get(id) {
                    assert_eq!(v1, v2);
                }
            }
            for (id, v1) in &self.loaded_runtime_objects {
                if let Some(v2) = loaded_runtime_objects.get(id) {
                    assert_eq!(v1, v2);
                }
            }
        }
        // Merge the two maps because we may be calling the execution engine more than once
        // (e.g. in advance epoch transaction, where we may be publishing a new system package).
        self.loaded_runtime_objects.extend(loaded_runtime_objects);
    }

    pub fn save_wrapped_object_containers(
        &mut self,
        wrapped_object_containers: BTreeMap<'a, ObjectId, ObjectId>,
    ) {
        self.invariants
            .save_wrapped_object_containers(wrapped_object_containers);
    }

    pub fn save_generated_object_ids(&mut self, generated_ids: BTreeSet<'a, ObjectId>) {
        self.invariants.save_generated_object_ids(generated_ids);
    }

    pub fn estimate_effects_size_upperbound(&self) -> usize {
        estimate_effects_size_upperbound_v2(
            self.execution_results.written_objects.len(),
            self.execution_results.modified_objects.len(),
            self.inputs.objects().len(),
        )
    }

    pub fn written_objects_size(&self) -> usize {
        self.execution_results
            .written_objects
            .values()
            .fold(0, |sum, obj| sum + obj.object_size_for_gas_metering())
    }

    /// Validates gasless post-execution requirements using the reservations cached when the store
    /// is constructed.
    pub(crate) fn check_gasless_execution_requirements(&self) -> Result<(), String> {
        // Gasless requirements are expressed in coin types `T`, while the shared input reservation
        // budget is keyed by accumulator types `Balance<T>`.
        let mut withdrawal_reservations = BTreeMap::new_in(self.bump);
        withdrawal_reservations.extend(
            self.post_execution_check_inputs
                .input_reservations
                .iter()
                .filter_map(|((owner, ty), amount)| {
                    accumulator_root::maybe_get_balance_type_param(ty)
                        .map(|coin_type| ((*owner, coin_type), *amount))
                }),
        );
        self.check_gasless_execution_requirements_with_reservations(Some(&withdrawal_reservations))
    }

    /// Validates gasless post-execution requirements:
    /// - No new objects were created or existing objects mutated (written_objects is empty)
    /// - The set of deleted objects exactly equals the set of input Coin objects
    /// - Each recipient receives at least the minimum transfer amount per token type
    /// - Unused withdrawal reservation (reservation - actual split) is 0 or >= min_amount
    pub(crate) fn check_gasless_execution_requirements_with_reservations(
        &self,
        withdrawal_reservations: Option<&BTreeMap<'_, (SuiAddress, TypeTag<'a>), u64>>,
    ) -> Result<(), String> {
        if !self.execution_results.written_objects.is_empty() {
            return Err("Gasless transactions cannot create or mutate objects".to_string());
        }

        let mut input_coin_ids = BTreeSet::new_in(self.bump);
        input_coin_ids.extend(
            self.inputs
                .objects()
                .iter()
                .filter(|(_, obj)| coin_type_maybe(obj))
                .map(|(id, _)| *id),
        );
        if self.execution_results.deleted_object_ids != input_coin_ids {
            return Err(format!(
                "Gasless transaction must destroy exactly its input Coins. \
                 Expected: {input_coin_ids:?}, deleted: {:?}",
                self.execution_results.deleted_object_ids
            ));
        }

        let allowed_types = get_gasless_allowed_token_types(self.bump, self.protocol_config);

        // Aggregate signed balance changes per (address, token_type).
        // Positive nets are recipient deposits that must meet the minimum transfer amount.
        let mut net_totals: BTreeMap<(SuiAddress, TypeTag<'a>), i128> = BTreeMap::new_in(self.bump);
        for (address, token_type, signed_amount) in
            signed_balance_changes_from_events(&self.execution_results.accumulator_events)
        {
            *net_totals.entry((address, token_type)).or_default() += signed_amount;
        }

        for ((recipient, token_type), net_amount) in &net_totals {
            if *net_amount <= 0 {
                continue;
            }
            if let Some(&min_amount) = allowed_types.get(token_type)
                && *net_amount < i128::from(min_amount)
            {
                return Err(format!(
                    "Gasless transfer of {net_amount} to {recipient} is below \
                     minimum {min_amount} for token type {}",
                    exec_types::type_tags::to_move_type_tag(token_type)
                ));
            }
        }

        if let Some(reservations) = withdrawal_reservations {
            for ((owner, token_type), &reserved) in reservations {
                let net = net_totals.get(&(*owner, *token_type)).copied().unwrap_or(0);
                let remaining = (reserved as i128).saturating_add(net);
                if remaining > 0
                    && let Some(&min_balance_remaining) = allowed_types.get(token_type)
                    && min_balance_remaining > 0
                    && remaining < min_balance_remaining as i128
                {
                    return Err(format!(
                        "Gasless withdrawal leaves {remaining} unused for {owner}, \
                         below minimum {min_balance_remaining} for token type {}",
                        exec_types::type_tags::to_move_type_tag(token_type)
                    ));
                }
            }
        }

        Ok(())
    }

    /// If there are unmetered storage rebate (due to system transaction), we put them into
    /// the storage rebate of 0x5 object.
    /// TODO: This will not work for potential future new system transactions if 0x5 is not in the input.
    /// We should fix this.
    pub fn conserve_unmetered_storage_rebate(&mut self, unmetered_storage_rebate: u64) {
        if unmetered_storage_rebate == 0 {
            // If unmetered_storage_rebate is 0, we are most likely executing the genesis transaction.
            // And in that case we cannot mutate the 0x5 object because it's newly created.
            // And there is no storage rebate that needs distribution anyway.
            return;
        }
        let system_state_wrapper = *self
            .read_object(&SUI_SYSTEM_STATE_OBJECT_ID)
            .expect("0x5 object must be mutated in system tx with unmetered storage rebate");
        // In unmetered execution, storage_rebate field of mutated object must be 0.
        // If not, we would be dropping SUI on the floor by overriding it.
        assert_eq!(system_state_wrapper.storage_rebate(), 0);
        self.mutate_input_object(
            system_state_wrapper.with_storage_rebate(unmetered_storage_rebate),
        );
    }

    /// Add an accumulator event to the execution results.
    pub fn add_accumulator_event(&mut self, event: AccumulatorEvent<'a>) {
        self.execution_results.accumulator_events.push(event);
    }

    /// Given an object ID, if it's not modified, returns None.
    /// Otherwise returns its metadata, including version, digest, owner and storage rebate.
    /// A modified object must be either a mutable input, or a loaded child object.
    /// The only exception is when we upgrade system packages, in which case the upgraded
    /// system packages are not part of input, but are modified.
    fn get_object_modified_at(
        &self,
        object_id: &ObjectId,
    ) -> Option<DynamicallyLoadedObjectMetadata<'a>> {
        if self.execution_results.modified_objects.contains(object_id) {
            Some(
                self.inputs
                    .exclusive_mutable_inputs()
                    .get(object_id)
                    .map(
                        |((version, digest), owner)| DynamicallyLoadedObjectMetadata {
                            version: *version,
                            digest: *digest,
                            owner: *owner,
                            // It's guaranteed that a mutable input object is an input object.
                            storage_rebate: self.inputs.objects()[object_id].storage_rebate(),
                            previous_transaction: self.inputs.objects()[object_id]
                                .previous_transaction(),
                        },
                    )
                    .or_else(|| self.loaded_runtime_objects.get(object_id).copied())
                    .unwrap_or_else(|| {
                        debug_assert!(is_system_package(object_id));
                        let obj = self.store.get_package_object(object_id).unwrap().unwrap();
                        DynamicallyLoadedObjectMetadata {
                            version: obj.version(),
                            digest: obj.digest(),
                            owner: *obj.owner(),
                            storage_rebate: obj.storage_rebate(),
                            previous_transaction: obj.previous_transaction(),
                        }
                    }),
            )
        } else {
            None
        }
    }

    pub fn protocol_config(&self) -> &'a ProtocolConfig {
        self.protocol_config
    }

    /// Run the (read-only) SUI-conservation and balance-accumulator invariant checks.
    /// See [`invariants::InvariantChecker::check_conservation_invariants`].
    pub(crate) fn check_conservation_invariants<Mode: ExecutionMode>(
        &self,
        move_vm: &Arc<MoveRuntime>,
        enable_expensive_checks: bool,
        cost_summary: &GasCostSummary,
    ) -> Result<(), ExecutionError<'a>> {
        self.invariants.check_conservation_invariants::<Mode>(
            self,
            move_vm,
            enable_expensive_checks,
            cost_summary,
        )
    }

    /// Check that every modified object traces back to an authenticated owner.
    /// See [`invariants::InvariantChecker::check_ownership_invariants`].
    /// See [`invariants::InvariantChecker::check_published_packages`].
    pub(crate) fn check_published_packages(&self) -> Result<(), ExecutionError<'a>> {
        self.invariants.check_published_packages(self)
    }

    pub(crate) fn check_ownership_invariants(
        &self,
        sender: &SuiAddress,
        sponsor: &Option<SuiAddress>,
        gas_charger: &GasCharger<'a>,
        is_epoch_change: bool,
    ) -> SuiResult<()> {
        self.invariants.check_ownership_invariants(
            self,
            sender,
            sponsor,
            gas_charger,
            is_epoch_change,
        )
    }
}

impl<'a> TemporaryStore<'a> {
    /// Track storage gas for each mutable input object (including the gas coin)
    /// and each created object. Compute storage refunds for each deleted object.
    /// Will *not* charge anything, gas status keeps track of storage cost and rebate.
    /// All objects will be updated with their new (current) storage rebate/cost.
    /// `SuiGasStatus` `storage_rebate` and `storage_gas_units` track the transaction
    /// overall storage rebate and cost.
    pub(crate) fn collect_storage_and_rebate(
        &mut self,
        gas_charger: &mut GasCharger<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        // Use two loops because we cannot mut iterate written while calling get_object_modified_at.
        let mut old_storage_rebates =
            Vec::with_capacity_in(self.execution_results.written_objects.len(), self.bump);
        old_storage_rebates.extend(self.execution_results.written_objects.keys().map(
            |object_id| {
                self.get_object_modified_at(object_id)
                    .map(|metadata| metadata.storage_rebate)
                    .unwrap_or_default()
            },
        ));
        assert_invariant!(
            old_storage_rebates.len() == self.execution_results.written_objects.len(),
            "a storage rebate for each written object"
        );
        // The lengths are checked equal above, which `zip_debug_eq` would also check.
        #[allow(clippy::disallowed_methods)]
        for (object, old_storage_rebate) in self
            .execution_results
            .written_objects
            .values_mut()
            .zip(old_storage_rebates)
        {
            // new object size
            let new_object_size = object.object_size_for_gas_metering();
            // track changes and compute the new object `storage_rebate`
            let new_storage_rebate = gas_charger
                .track_storage_mutation(object.id(), new_object_size, old_storage_rebate)
                .ok_or_else(|| ExecutionError::from_kind(ExecutionErrorKind::InvariantViolation))?;
            *object = object.with_storage_rebate(new_storage_rebate);
        }

        self.collect_rebate(gas_charger)
    }

    pub(crate) fn collect_rebate(
        &self,
        gas_charger: &mut GasCharger<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        for object_id in &self.execution_results.modified_objects {
            if self
                .execution_results
                .written_objects
                .contains_key(object_id)
            {
                continue;
            }
            // get and track the deleted object `storage_rebate`
            let storage_rebate = self
                .get_object_modified_at(object_id)
                // Unwrap is safe because this loop iterates through all modified objects.
                .unwrap()
                .storage_rebate;
            gas_charger
                .track_storage_mutation(*object_id, 0, storage_rebate)
                .ok_or_else(|| ExecutionError::from_kind(ExecutionErrorKind::InvariantViolation))?;
        }
        Ok(())
    }

    pub fn check_execution_results_consistency(&self) -> Result<(), ExecutionError<'a>> {
        assert_invariant!(
            self.execution_results
                .created_object_ids
                .iter()
                .all(|id| !self.execution_results.deleted_object_ids.contains(id)
                    && !self.execution_results.modified_objects.contains(id)),
            "Created object IDs cannot also be deleted or modified"
        );
        assert_invariant!(
            self.execution_results.modified_objects.iter().all(|id| {
                self.inputs.exclusive_mutable_inputs().contains_key(id)
                    || self.loaded_runtime_objects.contains_key(id)
                    || is_system_package(id)
            }),
            "A modified object must be either a mutable input, a loaded child object, or a system package"
        );
        Ok(())
    }
}

impl<'a> BackingPackageStore<'a> for TemporaryStore<'a> {
    fn get_package_object(&self, package_id: &ObjectId) -> SuiResult<Option<Object<'a>>> {
        // We first check the objects in the temporary store because in non-production code path,
        // it is possible to read packages that are just written in the same transaction.
        // This can happen for example when we run the expensive conservation checks, where we may
        // look into the types of each written object in the output, and some of them need the
        // newly written packages for type checking.
        // In production path though, this should never happen.
        if let Some(obj) = self.execution_results.written_objects.get(package_id) {
            Ok(Some(*obj))
        } else {
            self.store.get_package_object(package_id)
        }
    }
}

impl<'a> RuntimeObjectResolver<'a> for TemporaryStore<'a> {
    fn read_child_object(
        &self,
        parent: &ObjectId,
        child: &ObjectId,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object<'a>>> {
        let obj_opt = self.execution_results.written_objects.get(child);
        if obj_opt.is_some() {
            Ok(obj_opt.copied())
        } else {
            self.store
                .read_child_object(parent, child, child_version_upper_bound)
        }
    }

    fn get_object_received_at_version(
        &self,
        owner: &ObjectId,
        receiving_object_id: &ObjectId,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object<'a>>> {
        // You should never be able to try and receive an object after deleting it or writing it in the same
        // transaction since `Receiving` doesn't have copy.
        debug_assert!(
            !self
                .execution_results
                .written_objects
                .contains_key(receiving_object_id)
        );
        debug_assert!(
            !self
                .execution_results
                .deleted_object_ids
                .contains(receiving_object_id)
        );
        self.store.get_object_received_at_version(
            owner,
            receiving_object_id,
            receive_object_at_version,
            epoch_id,
        )
    }
}

impl ObjectFundsResolver for TemporaryStore<'_> {
    /// Loads the object balance at the required version and subtracts withdrawals from the same
    /// checkpoint that have not settled yet.
    /// This function is expected never to fail; an error indicates an invariant violation.
    fn object_available_balance(&self, owner: SuiAddress, type_: &TypeTag<'_>) -> SuiResult<u128> {
        let required_version = self
            .load_implicitly_read_system_object(&exec_types::base::SUI_ACCUMULATOR_ROOT_OBJECT_ID)
            .ok_or_else(|| SuiError("ExecutionInvariantViolation".to_string()))?
            .version();

        let settled =
            accumulator_root::load(self, Some(required_version), &owner, type_)?.unwrap_or(0);

        let unsettled = self.unsettled_object_funds.get_unsettled_object_withdraw(
            &accumulator_root::get_field_id(&owner, type_)?,
            required_version,
        );
        settled
            .checked_sub(unsettled)
            .ok_or_else(|| SuiError("ExecutionInvariantViolation".to_string()))
    }
}

/// Whether the change leaves the object without a live version: the effects' `deleted`
/// (`Exist → NotExist`, `Deleted`), `wrapped` (`Exist → NotExist`, `None`) and
/// `unwrapped_then_deleted` (`NotExist → NotExist`, `Deleted`).
fn removes_live_version(change: &EffectsObjectChange<'_>) -> bool {
    use messages::effects::{IdOperation, ObjectIn, ObjectOut};
    matches!(
        (
            &change.input_state,
            &change.output_state,
            change.id_operation
        ),
        (
            ObjectIn::Exist { .. },
            ObjectOut::NotExist,
            IdOperation::Deleted | IdOperation::None
        ) | (
            ObjectIn::NotExist,
            ObjectOut::NotExist,
            IdOperation::Deleted
        )
    )
}

/// `MoveObjectType::coin_type_maybe(..).is_some()` for the object's type.
fn coin_type_maybe(object: &Object<'_>) -> bool {
    matches!(
        object.type_(),
        Some(messages::object::MoveObjectType::GasCoin | messages::object::MoveObjectType::Coin(_))
    )
}

/// `TransactionKind::get_advance_epoch_tx_gas_summary`.
fn get_advance_epoch_tx_gas_summary(transaction_kind: &TransactionKind<'_>) -> Option<(u64, u64)> {
    let e = match transaction_kind {
        TransactionKind::ChangeEpoch(e) => e.get(),
        TransactionKind::EndOfEpochTransaction(txns) => {
            match txns
                .last()
                .expect("at least one end-of-epoch transaction required")
            {
                messages::system_transaction::EndOfEpochTransactionKind::ChangeEpoch(e) => e.get(),
                _ => panic!("final end-of-epoch txn must be ChangeEpoch"),
            }
        }
        _ => return None,
    };
    Some((e.computation_charge + e.storage_charge, e.storage_rebate))
}

/// Compute the per-`(address, type)` funds-accumulator reservation budget authorized by the
/// transaction, and the allowance ids declared per key. Today every funds accumulator is a
/// `Balance<T>`, but the `(address, TypeTag)` keying lets this generalize as more accumulator
/// types are added. Budget sources:
/// - PTB `FundsWithdrawalArg`s for any supported accumulator type (sender, sponsor, or
///   allowance funder as owner).
/// - Gas paid entirely from address balance (credits `(gas_owner, Balance<SUI>)`).
/// - Gas-data entries with coin-reservation digests (also credit `(gas_owner, Balance<SUI>)`).
fn compute_input_reservations<'a>(
    bump: &'a Bump,
    transaction_kind: &TransactionKind<'a>,
    gas_data: &GasData<'a>,
    transaction_signer: SuiAddress,
    enable_gasless: bool,
) -> (
    BTreeMap<'a, (SuiAddress, TypeTag<'a>), u64>,
    AllowanceIds<'a>,
) {
    let is_gasless = enable_gasless && is_gasless_transaction(gas_data, transaction_kind);
    let mut reservations: BTreeMap<(SuiAddress, TypeTag<'a>), u64> = BTreeMap::new_in(bump);
    let mut allowance_ids = AllowanceIds::new_in(bump);
    let sui_balance_type = accumulator_root::sui_balance_type(bump);

    for arg in get_funds_withdrawals(transaction_kind) {
        let WithdrawalTypeArg::Balance(inner) = arg.type_arg;
        let ty = accumulator_root::balance_type(bump, inner);
        let owner = match arg.withdraw_from {
            WithdrawFrom::Sender => transaction_signer,
            WithdrawFrom::Sponsor => *gas_data.owner,
            // The funder will differ from the signer/sponsor, but permission
            // is verified at signing
            WithdrawFrom::SenderAllowance { funder, allowance } => {
                allowance_ids
                    .entry((*funder, ty))
                    .or_insert_with(|| Vec::new_in(bump))
                    .push(*allowance);
                *funder
            }
        };
        let Reservation::MaxAmountU64(reservation) = arg.reservation;
        let entry = reservations.entry((owner, ty)).or_insert(0);
        *entry = entry.saturating_add(reservation);
    }

    // Gasless transactions charge no gas, so gas sources grant no reservation (their budget is
    // validated to be 0 anyway; skipping keeps the map free of a phantom zero entry).
    if !is_gasless && is_gas_paid_from_address_balance(gas_data, transaction_kind) {
        let entry = reservations
            .entry((*gas_data.owner, sui_balance_type))
            .or_insert(0);
        *entry = entry.saturating_add(gas_data.budget);
    }

    for entry in gas_data.payment {
        // `ParsedDigest::try_from`: a coin reservation's digest carries its amount.
        if entry.is_coin_reservation() {
            let amount = u64::from_le_bytes(
                entry.digest.bytes[0..8]
                    .try_into()
                    .expect("a digest has eight bytes"),
            );
            let entry = reservations
                .entry((*gas_data.owner, sui_balance_type))
                .or_insert(0);
            *entry = entry.saturating_add(amount);
        }
    }

    (reservations, allowance_ids)
}

/// What each `Publish`/`Upgrade` command declares about the package it writes, in command order.
/// `None` for transaction kinds that are not PTBs.
fn declared_packages<'a>(
    bump: &'a Bump,
    transaction_kind: &TransactionKind<'a>,
) -> Option<Vec<'a, (usize, BTreeSet<'a, ObjectId>)>> {
    let TransactionKind::ProgrammableTransaction(pt) = transaction_kind else {
        return None;
    };
    let mut declared = Vec::new_in(bump);
    for command in pt.commands {
        match command {
            Command::Publish(modules, dep_ids) | Command::Upgrade(modules, dep_ids, _, _) => {
                let mut deps = BTreeSet::new_in(bump);
                deps.extend(dep_ids.iter().copied());
                declared.push((modules.len(), deps));
            }
            _ => (),
        }
    }
    Some(declared)
}

/// Compares the owner and payload of an object.
/// This is used to detect illegal writes to non-exclusive write objects.
fn was_object_mutated(object: &Object<'_>, original: &Object<'_>) -> bool {
    let data_equal = match (object.data(), original.data()) {
        (Data::Move(a), Data::Move(b)) => a.contents == b.contents && a.type_ == b.type_,
        // We don't have a use for package content-equality, so we remain as strict as
        // possible for now.
        (Data::Package(a), Data::Package(b)) => a == b,
        _ => false,
    };

    let owner_equal = match (object.owner(), original.owner()) {
        // We don't compare initial shared versions, because re-shared objects do not have the
        // correct initial shared version at this point in time, and this field is not something
        // that can be modified by a single transaction anyway.
        (Owner::Shared { .. }, Owner::Shared { .. }) => true,
        (
            Owner::ConsensusAddressOwner { owner: a, .. },
            Owner::ConsensusAddressOwner { owner: b, .. },
        ) => a == b,
        (Owner::AddressOwner(a), Owner::AddressOwner(b)) => a == b,
        (Owner::Immutable, Owner::Immutable) => true,
        (Owner::ObjectOwner(a), Owner::ObjectOwner(b)) => a == b,
        // The reference compares the parties' permissions, not their start versions.
        (Owner::Party(a), Owner::Party(b)) => {
            a.default_permissions == b.default_permissions && a.members == b.members
        }

        // Keep the left hand side of the match exhaustive to catch future
        // changes to Owner
        (Owner::AddressOwner(_), _)
        | (Owner::Immutable, _)
        | (Owner::ObjectOwner(_), _)
        | (Owner::Shared { .. }, _)
        | (Owner::ConsensusAddressOwner { .. }, _)
        | (Owner::Party(_), _) => false,
    };

    !data_equal || !owner_equal
}

impl<'a> Storage<'a> for TemporaryStore<'a> {
    fn reset(&mut self) {
        self.drop_writes();
    }

    fn read_object(&self, id: &ObjectId) -> Option<&Object<'a>> {
        TemporaryStore::read_object(self, id)
    }

    /// Take execution results v2, and translate it back to be compatible with effects v1.
    fn record_execution_results(
        &mut self,
        mut results: ExecutionResultsV2<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        // for all non-exclusive write inputs, remove them from written objects
        let inputs = self.inputs;
        let mut to_remove = Vec::new_in(self.bump);
        for (id, original) in inputs.non_exclusive_input_objects() {
            // Object must be present in `written_objects` and identical
            if results
                .written_objects
                .get(id)
                .map(|obj| was_object_mutated(obj, original))
                .unwrap_or(true)
            {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::NonExclusiveWriteInputObjectModified {
                        id: containers::alloc(self.bump, *id),
                    },
                    "Non-exclusive write input object has been modified or deleted",
                ));
            }
            to_remove.push(*id);
        }

        for id in to_remove {
            results.written_objects.remove(&id);
            results.modified_objects.remove(&id);
        }

        // It's important to merge instead of override results because it's
        // possible to execute PT more than once during tx execution.
        // Track the index range of accumulator events brought in here as PTB-emitted; the
        // address-balance change invariant (run inside `run_conservation_checks`) uses this
        // set to distinguish trusted PTB-emitted events from runtime-emitted ones.
        let event_start = self.execution_results.accumulator_events.len();
        self.execution_results.merge_results(
            results, /* consistent_merge */ true, /* invariant_checks */ true,
        )?;
        let event_end = self.execution_results.accumulator_events.len();
        self.invariants
            .record_ptb_event_range(event_start, event_end);

        Ok(())
    }

    fn save_loaded_runtime_objects(
        &mut self,
        loaded_runtime_objects: BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    ) {
        TemporaryStore::save_loaded_runtime_objects(self, loaded_runtime_objects)
    }

    fn save_wrapped_object_containers(
        &mut self,
        wrapped_object_containers: BTreeMap<'a, ObjectId, ObjectId>,
    ) {
        TemporaryStore::save_wrapped_object_containers(self, wrapped_object_containers)
    }

    fn check_coin_deny_list(
        &self,
        receiving_funds_type_and_owners: BTreeMap<'a, TypeTag<'a>, BTreeSet<'a, SuiAddress>>,
    ) -> DenyListResult<'a> {
        let result = check_coin_deny_list_v2_during_execution(
            self.bump,
            receiving_funds_type_and_owners,
            self.cur_epoch,
            self.store,
        );
        // The denylist object is only loaded if there are regulated transfers.
        // And also if we already have it in the input there is no need to commit it again in the effects.
        if result.num_non_gas_coin_owners > 0
            && !self.inputs.objects().contains_key(&SUI_DENY_LIST_OBJECT_ID)
        {
            self.loaded_per_epoch_config_objects
                .borrow_mut()
                .insert(SUI_DENY_LIST_OBJECT_ID);
        }
        result
    }

    fn record_generated_object_ids(&mut self, generated_ids: BTreeSet<'a, ObjectId>) {
        TemporaryStore::save_generated_object_ids(self, generated_ids)
    }
}
