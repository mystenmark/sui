// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Move tracing (`trace_utils`, `MoveTraceBuilder`) is not ported: the reference's trace
//! parameters and calls are left out here and in the interpreter.

use crate::{
    accumulator_event::AccumulatorEvent,
    accumulator_root, adapter,
    data_store::transaction_package_store::into_serialized_move_package,
    execution::ExecutionResultsV2,
    execution_mode::ExecutionMode,
    execution_value::ExecutionState,
    gas_charger::{GasCharger, GasPayment, PaymentLocation},
    gas_meter::SuiGasMeter,
    move_package, sp,
    static_programmable_transactions::{
        env::Env,
        execution::values::{Local, Locals, UpgradeCap, UpgradeReceipt, UpgradeTicket, Value},
        linkage::resolved_linkage::{ExecutableLinkage, ResolvedLinkage},
        loading::ast::{Datatype, DeserializedPackage, ModuleId, PackagePayload},
        typing::ast::{self as T, Type},
    },
    storage::DenyListResult,
};
use containers::{
    BTreeMap, BTreeSet, Bump, IndexMap, IndexSet, Vec, alloc, alloc_slice_copy, alloc_str,
};
use exec_types::base::{move_address, object_id};
use exec_types::error::{ExecutionError, ExecutionErrorKind, SafeIndex, command_argument_error};
use exec_types::execution::DynamicallyLoadedObjectMetadata;
use exec_types::object::{Object, get_owner_address, original_package_id};
use exec_types::storage::{BackingPackageStore, ObjectFundsResolver, RuntimeObjectResolver};
use exec_types::tx_context::TxContext;
use exec_types::type_tags::{
    move_object_type_all_addresses, move_object_type_of, move_object_type_struct_tag_in,
    to_move_struct_tag, to_move_struct_tag_of,
};
use exec_types::{assert_invariant, checked_as, invariant_violation, make_invariant_violation};
use messages::base::{ObjectId, SequenceNumber, SuiAddress, U64Le};
use messages::effects::{AccumulatorValue, AccumulatorWriteV1, Event, EventCommitment};
use messages::execution_status::{CommandArgumentError, PackageUpgradeError};
use messages::object::{MoveObject, MoveObjectType, MovePackage, Owner};
use messages::type_tag::{StructTag, TypeTag};
use move_binary_format::{
    CompiledModule,
    compatibility::{Compatibility, InclusionCheck},
    errors::{Location, PartialVMError, PartialVMResult, VMResult},
    file_format::FunctionDefinitionIndex,
    normalized,
};
use move_core_types::{
    account_address::AccountAddress, identifier::IdentStr, language_storage as move_tags,
    u256::U256,
};
use move_vm_runtime::{
    execution::{
        Type as VMType, TypeSubst as _,
        values::{VMValueCast, Value as VMValue},
        vm::{LoadedFunctionInformation, MoveVM},
    },
    natives::extensions::NativeExtensions,
    shared::gas::{GasMeter as _, SimpleInstruction},
    validation::verification::ast::Package as VerifiedPackage,
};
use natives::NativesCostTable;
use natives::object_runtime::{
    self, LoadedRuntimeObject, MoveAccumulatorAction, MoveAccumulatorEvent, MoveAccumulatorValue,
    ObjectRuntime, RuntimeResults, get_all_uids, max_event_error,
};
use serde::Deserialize;
use std::{cell::RefCell, fmt, rc::Rc, sync::Arc};
use sui_protocol_config::ProtocolConfig;
use sui_types::{
    accumulator_root::SETTLEMENT_MAX_TYPE_INSTANTIATION_NODES,
    base_types::{RESOLVED_ASCII_STR, RESOLVED_UTF8_STR},
    metrics::ExecutionMetrics,
    move_package::UpgradePolicy,
    object::ObjectPermissions,
};
use sui_verifier::INIT_FN_NAME;

/// Publish init runs before any command arguments are read, so the gas stack is still empty.
const PUBLISH_INIT_EXPECTED_STACK_HEIGHT: u64 = 0;
/// Upgrade init runs after the upgrade ticket argument has been read. Command stack balancing
/// happens later in the interpreter, after the upgrade command returns.
const UPGRADE_INIT_EXPECTED_STACK_HEIGHT: u64 = 1;

macro_rules! unwrap {
    ($e:expr, $($args:expr),* $(,)?) => {
        match $e {
            Some(v) => v,
            None => {
                invariant_violation!("Unexpected none: {}", format!($($args),*))
            }
        }

    };
}

#[macro_export]
macro_rules! object_runtime {
    ($context:ident) => {
        $context
            .native_extensions
            .try_borrow()
            .map_err(|_| {
                exec_types::make_invariant_violation!(
                    "Should be able to borrow object runtime native extension"
                )
            })?
            .get::<natives::object_runtime::ObjectRuntime>()
            .map_err(|e| {
                $context
                    .env
                    .convert_vm_error(e.finish(move_binary_format::errors::Location::Undefined))
            })
    };
}

macro_rules! object_runtime_mut {
    ($context:ident) => {
        $context
            .native_extensions
            .try_borrow_mut()
            .map_err(|_| {
                make_invariant_violation!(
                    "Should be able to borrow object runtime native extension"
                )
            })?
            .get_mut::<ObjectRuntime>()
            .map_err(|e| $context.env.convert_vm_error(e.finish(Location::Undefined)))
    };
}

macro_rules! charge_gas_ {
    ($gas_charger:expr, $env:expr, $call:ident($($args:expr),*)) => {{
        SuiGasMeter($gas_charger.move_gas_status_mut())
            .$call($($args),*)
            .map_err(|e| $env.convert_vm_error(e.finish(Location::Undefined)))
    }};
    ($gas_charger:expr, $env:expr, $case:ident, $value_view:expr) => {
        charge_gas_!($gas_charger, $env, $case($value_view))
    };
}

macro_rules! charge_gas {
    ($context:ident, $case:ident, $value_view:expr) => {{ charge_gas_!($context.gas_charger, $context.env, $case, $value_view) }};
}

// Helper macro to manage Move VM cache for different linkage contexts. If the given linkage is
// found the VM is reused, otherwise a new VM is created and inserted into the cache.
//
// The reference keys the cache by `LinkageHash`, the linkage table as addresses, built on every
// call. Here an entry is found by its `ResolvedLinkage`'s `linkage` map, which converts to that
// table one to one, so equal maps are equal keys. The `LinkageContext` is built only on a miss: a
// hit means an equal table already passed `LinkageContext::new`, which is deterministic, so it
// would pass again. As in the reference, the VM is out of the cache while the body runs and is
// put back only if the body succeeds.
macro_rules! with_vm {
    ($self:ident, $linkage:expr, $body:expr) => {{
        let linkage: &ExecutableLinkage<'a> = $linkage;
        let cached = $self
            .executable_vm_cache
            .iter()
            .position(|(l, _)| std::ptr::eq(*l, linkage.0) || l.linkage == linkage.0.linkage);
        let mut vm = if let Some(i) = cached {
            let (_, vm) = $self.executable_vm_cache.swap_remove(i);
            debug_assert_eq!(
                linkage.linkage_context().ok().as_ref(),
                Some(vm.linkage_context()),
                "a cached VM's linkage differs from the linkage it was found by"
            );
            vm
        } else {
            let link_context = linkage.linkage_context()?;
            let data_store = &$self.env.linkable_store.package_store;
            $self
                .env
                .vm
                .make_vm_with_native_extensions(
                    data_store,
                    link_context,
                    $self.native_extensions.clone(),
                )
                .map_err(|e| $self.env.convert_linked_vm_error(e, linkage))?
        };
        let result = $body(&mut vm)?;
        // The body never makes a VM itself, so no equal entry was added meanwhile.
        debug_assert!(
            $self
                .executable_vm_cache
                .iter()
                .all(|(l, _)| l.linkage != linkage.0.linkage)
        );
        $self.executable_vm_cache.push((linkage.0, vm));
        Ok(result)
    }};
}

/// Type wrapper around Value to ensure safe usage
#[derive(Debug)]
pub struct CtxValue(Value);

#[derive(Clone, Copy, Debug)]
pub struct InputObjectMetadata<'a> {
    pub newly_created: bool,
    pub id: ObjectId,
    pub refined_permissions: ObjectPermissions,
    pub owner: Owner<'a>,
    pub version: SequenceNumber,
    pub type_: Type<'a>,
}

/// Metadata in the case that the GasCoin is transferred, either as an object to another recipient
/// or as an address balance via `sui::coin::send_funds`. This is needed at the end to
/// both refund the gas budget and set the correct location from which to charge gas.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GasCoinTransfer {
    /// Sent using the TransferObjects command
    TransferObjects,
    /// Sent with the `sui::coin::send_funds` command
    SendFunds {
        /// The recipient for `send_funds`.
        recipient: AccountAddress,
    },
}

#[derive(Copy, Clone)]
enum UsageKind {
    Move,
    Copy,
    Borrow,
}

// Locals and metadata for all `Location`s. Separated from `Context` for lifetime reasons.
struct Locations<'a> {
    // A single local for holding the TxContext
    tx_context_value: Locals<'a>,
    /// The runtime value for the Gas coin, None if no gas coin is provided
    gas: Option<(GasPayment, InputObjectMetadata<'a>, Locals<'a>)>,
    /// The runtime value for the input objects args
    input_object_metadata: Vec<'a, (T::InputIndex, InputObjectMetadata<'a>)>,
    object_inputs: Locals<'a>,
    input_withdrawal_metadata: Vec<'a, T::WithdrawalInput<'a>>,
    withdrawal_inputs: Locals<'a>,
    pure_input_bytes: IndexSet<'a, &'a [u8]>,
    pure_input_metadata: Vec<'a, T::PureInput<'a>>,
    pure_inputs: Locals<'a>,
    receiving_input_metadata: Vec<'a, T::ReceivingInput<'a>>,
    receiving_inputs: Locals<'a>,
    /// The results of a given command. For most commands, the inner vector will have length 1.
    /// It will only not be 1 for Move calls with multiple return values.
    /// Inner values are None if taken/moved by-value
    results: Vec<'a, Locals<'a>>,
}

enum ResolvedLocation<'l, 'a> {
    Local(Local<'l, 'a>),
    Pure {
        bytes: &'a [u8],
        metadata: &'l T::PureInput<'a>,
        local: Local<'l, 'a>,
    },
    Receiving {
        metadata: &'l T::ReceivingInput<'a>,
        local: Local<'l, 'a>,
    },
}

/// Maintains all runtime state specific to programmable transactions
pub struct Context<'env, 'a, 'pc, 'vm, 'state, 'linkage, 'gas, 'extension, Mode>
where
    Mode: ExecutionMode,
{
    pub env: &'env Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
    /// Metrics for reporting exceeded limits
    pub metrics: Arc<ExecutionMetrics>,
    // Private to the crate, unlike the reference's: the object runtime in it holds the state view
    // for longer than the context borrows it (see `state_view_for_object_runtime`).
    pub(crate) native_extensions: NativeExtensions<'a>,
    /// A shared transaction context, contains transaction digest information and manages the
    /// creation of new object IDs
    pub tx_context: Rc<RefCell<TxContext>>,
    /// The gas charger used for metering
    pub gas_charger: &'gas mut GasCharger<'a>,
    /// User events are claimed after each Move call
    user_events: Vec<'a, (ModuleId<'a>, StructTag<'a>, &'a [u8])>,
    // runtime data
    locations: Locations<'a>,
    /// Tracks where the gas coin was sent, if it was moved by value
    gas_coin_transfer: Option<GasCoinTransfer>,
    // cache of Move VMs created this transaction for different linkage contexts so that we can reuse them.
    // The reference's is an LRU of 1024 VMs, more than a transaction's commands can make. A
    // transaction makes few, so they are searched linearly (see `with_vm!`).
    executable_vm_cache: Vec<'a, (&'a ResolvedLinkage<'a>, MoveVM<'a>)>,
}

impl<'a> Locations<'a> {
    /// NOTE! This does not charge gas and should not be used directly. It is exposed for
    /// dev-inspect
    fn resolve(
        &mut self,
        location: T::Location,
    ) -> Result<ResolvedLocation<'_, 'a>, ExecutionError<'a>> {
        Ok(match location {
            T::Location::TxContext => ResolvedLocation::Local(self.tx_context_value.local(0)?),
            T::Location::GasCoin => {
                let (_, _, gas_locals) = unwrap!(self.gas.as_mut(), "Gas coin not provided");
                ResolvedLocation::Local(gas_locals.local(0)?)
            }
            T::Location::ObjectInput(i) => ResolvedLocation::Local(self.object_inputs.local(i)?),
            T::Location::WithdrawalInput(i) => {
                ResolvedLocation::Local(self.withdrawal_inputs.local(i)?)
            }
            T::Location::Result(i, j) => {
                let result = unwrap!(self.results.get_mut(i as usize), "bounds already verified");
                ResolvedLocation::Local(result.local(j)?)
            }
            T::Location::PureInput(i) => {
                let local = self.pure_inputs.local(i)?;
                let metadata = self.pure_input_metadata.safe_get(i as usize)?;
                let bytes = *self
                    .pure_input_bytes
                    .get_index(metadata.byte_index)
                    .ok_or_else(|| {
                        make_invariant_violation!(
                            "Pure input {} bytes out of bounds at index {}",
                            metadata.original_input_index.0,
                            metadata.byte_index,
                        )
                    })?;
                ResolvedLocation::Pure {
                    bytes,
                    metadata,
                    local,
                }
            }
            T::Location::ReceivingInput(i) => ResolvedLocation::Receiving {
                metadata: self.receiving_input_metadata.safe_get(i as usize)?,
                local: self.receiving_inputs.local(i)?,
            },
        })
    }
}

/// The object runtime's resolvers: the state view, for the arena's lifetime.
///
/// The reference lends `env.state_view` to the object runtime for as long as the context lives,
/// which its `ObjectRuntime<'env>` can name. Here the object runtime has one lifetime, the
/// arena's, for both the views it records and the resolvers it reads through, so the borrow is
/// stretched to it.
///
/// # Safety
/// The returned references must not be used once the context's borrow of the state view ends.
/// They are held only by the object runtime in the context's native extensions, which are shared
/// only with the VMs the context makes and keeps (or consumes) itself, and which the crate never
/// hands out; all of them are dropped with the context, before the state view can be used
/// mutably again. Neither trait returns references into the state view: only owned values, whose
/// parts live for `'a` in their own right.
unsafe fn state_view_for_object_runtime<'s, 'a>(
    state_view: &'s (dyn ExecutionState<'a> + 's),
) -> (
    &'a dyn RuntimeObjectResolver<'a>,
    &'a dyn ObjectFundsResolver,
) {
    // SAFETY: the caller upholds the contract above; the layout of the reference is unchanged.
    let state_view: &'a (dyn ExecutionState<'a> + 'a) = unsafe {
        std::mem::transmute::<&'s (dyn ExecutionState<'a> + 's), &'a (dyn ExecutionState<'a> + 'a)>(
            state_view,
        )
    };
    (state_view, state_view)
}

impl<'env, 'a, 'pc, 'vm, 'state, 'linkage, 'gas, 'extension, Mode>
    Context<'env, 'a, 'pc, 'vm, 'state, 'linkage, 'gas, 'extension, Mode>
where
    Mode: ExecutionMode,
{
    pub fn new(
        env: &'env Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
        metrics: Arc<ExecutionMetrics>,
        tx_context: Rc<RefCell<TxContext>>,
        gas_charger: &'gas mut GasCharger<'a>,
        payment_location: Option<GasPayment>,
        pure_input_bytes: IndexSet<'a, &'a [u8]>,
        object_inputs: Vec<'a, T::ObjectInput<'a>>,
        input_withdrawal_metadata: Vec<'a, T::WithdrawalInput<'a>>,
        pure_input_metadata: Vec<'a, T::PureInput<'a>>,
        receiving_input_metadata: Vec<'a, T::ReceivingInput<'a>>,
        natives_cost_table: Option<&NativesCostTable>,
    ) -> Result<Self, ExecutionError<'a>>
    where
        'pc: 'a,
    {
        let bump = env.bump;
        let mut input_object_map = BTreeMap::new_in(bump);
        let mut input_object_metadata = Vec::with_capacity_in(object_inputs.len(), bump);
        let mut object_values = Vec::with_capacity_in(object_inputs.len(), bump);
        let mut input_layouts = InputLayouts(Vec::new_in(bump));
        for object_input in object_inputs {
            let (i, m, v) = load_object_arg(
                gas_charger,
                env,
                &mut input_object_map,
                &mut input_layouts,
                object_input,
            )?;
            input_object_metadata.push((i, m));
            object_values.push(Some(v));
        }
        let object_inputs = Locals::new(bump, object_values)?;
        let mut withdrawal_values = Vec::with_capacity_in(input_withdrawal_metadata.len(), bump);
        for withdrawal_input in &input_withdrawal_metadata {
            let v = load_withdrawal_arg(gas_charger, env, withdrawal_input)?;
            withdrawal_values.push(Some(v));
        }
        let withdrawal_inputs = Locals::new(bump, withdrawal_values)?;
        let pure_inputs = Locals::new_invalid(bump, pure_input_metadata.len())?;
        let receiving_inputs = Locals::new_invalid(bump, receiving_input_metadata.len())?;
        let mut new_gas_coin_id = None;
        let gas = match payment_location {
            Some(gas_payment)
                if matches!(gas_payment.location, PaymentLocation::AddressBalance(_))
                    && !env.protocol_config.gasless_transaction_drop_safety() =>
            {
                None
            }
            Some(gas_payment) => {
                let ty = env.gas_coin_type()?;
                let (gas_metadata, gas_value) = match gas_payment.location {
                    PaymentLocation::AddressBalance(sui_address) => {
                        assert_invariant!(
                            env.protocol_config.enable_address_balance_gas_payments(),
                            "Address balance gas payments must be enabled to have an address \
                             balance payment location"
                        );
                        let max_gas_in_balance = gas_charger.gas_budget();
                        assert_invariant!(
                            gas_payment.amount >= max_gas_in_balance,
                            "not enough gas to pay. How did we get this far?"
                        );
                        let id = tx_context.borrow_mut().fresh_id();
                        new_gas_coin_id = Some(id);

                        let metadata = InputObjectMetadata {
                            newly_created: true,
                            id,
                            refined_permissions: ObjectPermissions::ALL,
                            owner: Owner::AddressOwner(alloc(bump, sui_address)),
                            version: 0,
                            type_: ty,
                        };
                        let coin = Value::coin(id, gas_payment.amount);
                        (metadata, coin)
                    }
                    PaymentLocation::Coin(gas_coin_id) => load_object_arg_impl(
                        gas_charger,
                        env,
                        &mut input_object_map,
                        &mut input_layouts,
                        gas_coin_id,
                        ObjectPermissions::ALL,
                        ty,
                    )?,
                };
                let mut gas_locals = Locals::new(bump, [Some(gas_value)])?;
                let mut gas_local = gas_locals.local(0)?;
                let gas_ref = gas_local.borrow()?;
                // We have already checked that the gas balance is enough to cover the gas budget
                let max_gas_in_balance = gas_charger.gas_budget();
                gas_ref.coin_ref_subtract_balance(max_gas_in_balance)?;
                Some((gas_payment, gas_metadata, gas_locals))
            }
            None => None,
        };
        // SAFETY: the state view is borrowed by `env` for the context's lifetime, and the
        // resolvers live only in the native extensions, which go with the context.
        let (child_resolver, object_funds_resolver) =
            unsafe { state_view_for_object_runtime(&*env.state_view) };
        let native_extensions = adapter::new_native_extensions(
            bump,
            child_resolver,
            object_funds_resolver,
            input_object_map,
            !gas_charger.is_unmetered(),
            env.protocol_config,
            natives_cost_table,
            metrics.clone(),
            tx_context.clone(),
        )?;
        if let Some(new_gas_coin_id) = new_gas_coin_id {
            // If we created a new gas coin for the transaction,
            // we need to add it to the object runtime
            native_extensions
                .try_borrow_mut()
                .map_err(|_| {
                    make_invariant_violation!(
                        "Should be able to borrow object runtime native extension"
                    )
                })?
                .get_mut::<ObjectRuntime>()
                .and_then(|object_runtime| object_runtime.new_id(new_gas_coin_id))
                .map_err(|e| env.convert_vm_error(e.finish(Location::Undefined)))?;
        }

        debug_assert_eq!(gas_charger.move_gas_status().stack_height_current(), 0);
        let tx_context_value = Locals::new(
            bump,
            [Some(Value::new_tx_context(tx_context.borrow().digest())?)],
        )?;
        Ok(Self {
            env,
            metrics,
            native_extensions,
            tx_context,
            gas_charger,
            user_events: Vec::new_in(bump),
            locations: Locations {
                tx_context_value,
                gas,
                input_object_metadata,
                object_inputs,
                input_withdrawal_metadata,
                withdrawal_inputs,
                pure_input_bytes,
                pure_input_metadata,
                pure_inputs,
                receiving_input_metadata,
                receiving_inputs,
                results: Vec::new_in(bump),
            },
            gas_coin_transfer: None,
            executable_vm_cache: Vec::new_in(bump),
        })
    }

    pub(crate) fn record_gas_coin_transfer(
        &mut self,
        transfer: GasCoinTransfer,
    ) -> Result<(), ExecutionError<'a>> {
        // send funds transfer ==> accumulators/address balances are enabled
        assert_invariant!(
            !matches!(transfer, GasCoinTransfer::SendFunds { .. })
                || self.env.protocol_config.enable_accumulators(),
            "Gas coin transfers with send_funds are not allowed unless accumulators are enabled"
        );
        if self.gas_coin_transfer.is_some() {
            invariant_violation!("Gas coin destination set more than once");
        }
        self.gas_coin_transfer = Some(transfer);
        Ok(())
    }

    /// `loaded_runtime_objects` must be the object runtime's `loaded_runtime_objects()`, taken
    /// after the last command.
    pub fn finish(
        mut self,
        loaded_runtime_objects: &BTreeMap<'a, ObjectId, DynamicallyLoadedObjectMetadata<'a>>,
    ) -> Result<ExecutionResultsV2<'a>, ExecutionError<'a>> {
        let bump = self.env.bump;
        assert_invariant!(
            !self.locations.tx_context_value.local(0)?.is_invalid()?,
            "tx context value should be present"
        );
        let gas_coin_transfer = self.gas_coin_transfer;
        let gas = std::mem::take(&mut self.locations.gas);
        let object_input_metadata =
            std::mem::replace(&mut self.locations.input_object_metadata, Vec::new_in(bump));
        let mut object_inputs = std::mem::replace(
            &mut self.locations.object_inputs,
            Locals::new_invalid(bump, 0)?,
        );
        let mut created_input_object_ids = BTreeSet::new_in(bump);
        let child_loaded_runtime_objects = loaded_runtime_objects;
        let mut loaded_runtime_objects = BTreeMap::new_in(bump);
        let mut by_value_shared_objects = BTreeSet::new_in(bump);
        let mut consensus_owner_objects = BTreeMap::new_in(bump);
        let mut gas_payment_location = None;
        let gas = gas
            .map(|(payment_location, m, mut g)| {
                gas_payment_location = Some(payment_location);
                let value_opt = g.local(0)?.move_if_valid()?;
                let moved = value_opt.is_none();
                assert_invariant!(
                    moved == gas_coin_transfer.is_some(),
                    "Gas coin moved requires gas coin transfer to be recorded, and vice versa"
                );
                Result::<_, ExecutionError<'a>>::Ok((m, value_opt))
            })
            .transpose()?;

        let gas_id_opt = gas.as_ref().map(|(m, _)| m.id);
        let mut object_input_values = Vec::with_capacity_in(object_input_metadata.len(), bump);
        for (i, (_, m)) in object_input_metadata.into_iter().enumerate() {
            let v_opt = object_inputs.local(checked_as!(i, u16)?)?.move_if_valid()?;
            object_input_values.push((m, v_opt));
        }
        for (metadata, value_opt) in object_input_values.into_iter().chain(gas) {
            let InputObjectMetadata {
                newly_created,
                id,
                refined_permissions,
                owner,
                version,
                type_,
            } = metadata;
            if !refined_permissions.can_use_mutably() {
                continue;
            }

            if newly_created {
                created_input_object_ids.insert(id);
            } else {
                loaded_runtime_objects.insert(
                    id,
                    LoadedRuntimeObject {
                        version,
                        is_modified: true,
                    },
                );
            }
            if let Some(object) = value_opt {
                self.transfer_object_(
                    owner,
                    type_,
                    CtxValue(object),
                    /* end of transaction */ true,
                )?;
            } else if matches!(owner, Owner::Shared { .. }) {
                by_value_shared_objects.insert(id);
            } else if matches!(owner, Owner::ConsensusAddressOwner { .. }) {
                consensus_owner_objects.insert(id, owner);
            }
        }

        let Self {
            env,
            native_extensions,
            tx_context,
            gas_charger,
            user_events,
            ..
        } = self;
        let ref_context: &RefCell<TxContext> = &tx_context;
        let tx_context: &TxContext = &ref_context.borrow();
        let tx_digest = ref_context.borrow().digest();

        let object_runtime: ObjectRuntime = native_extensions
            .try_borrow_mut()
            .map_err(|_| {
                make_invariant_violation!(
                "Should be able to borrow object runtime native extension at the end of execution"
            )
            })?
            .remove()
            .map_err(|e| env.convert_vm_error(e.finish(Location::Undefined)))?;

        let RuntimeResults {
            mut writes,
            user_events: remaining_events,
            loaded_child_objects,
            mut created_object_ids,
            deleted_object_ids,
            mut accumulator_events,
            settlement_input_sui,
            settlement_output_sui,
        } = object_runtime.finish(child_loaded_runtime_objects)?;
        assert_invariant!(
            loaded_runtime_objects
                .keys()
                .all(|id| !created_object_ids.contains(id)),
            "Loaded input objects should not be in the created objects set"
        );
        // TODO generalize post transaction checks using permissions

        assert_invariant!(
            remaining_events.is_empty(),
            "Events should be taken after every Move call"
        );
        // Refund unused gas to the coin, real or ephemeral
        if let Some(gas_id) = gas_id_opt {
            // deleted implies was moved and used in send_funds
            assert_invariant!(
                !deleted_object_ids.contains(&gas_id)
                    || gas_coin_transfer.is_some_and(|destination| matches!(
                        destination,
                        GasCoinTransfer::SendFunds { .. }
                    )),
                "Gas coin should not be deleted"
            );
            let Some(gas_payment_location) = gas_payment_location else {
                invariant_violation!("Gas payment should be specified if gas ID is present");
            };
            finish_gas_coin(
                bump,
                gas_charger,
                &mut writes,
                &mut created_object_ids,
                &deleted_object_ids,
                &mut accumulator_events,
                gas_id,
                gas_payment_location,
                gas_coin_transfer,
            )?;
        }

        loaded_runtime_objects.extend(loaded_child_objects);

        let mut written_objects = BTreeMap::new_in(bump);

        let (writeout_vm, ty_linkage) =
            Self::make_writeout_vm(env, writes.values().map(|(_, ty, _)| *ty))?;

        // The type and layout of each object type written, which the reference loads for every
        // write. A hit is never false: equal `MoveObjectType`s are the same struct tag, and the
        // type and layout are a deterministic function of the tag in the one write-out VM.
        let mut writeout_types = Vec::new_in(bump);
        for (id, (recipient, object_type, value)) in writes {
            let (ty, layout) = match writeout_types.iter().find(|(t, _, _)| *t == object_type) {
                Some((_, ty, layout)) => {
                    debug_assert!(
                        Self::load_type_and_layout_from_struct_for_writeout(
                            env,
                            &writeout_vm,
                            &ty_linkage,
                            to_move_struct_tag_of(&object_type),
                        )
                        .is_ok_and(
                            |(t, l)| t == *ty && object_runtime::runtime_layouts_equal(&l, layout)
                        )
                    );
                    (*ty, layout)
                }
                None => {
                    let (ty, layout) = Self::load_type_and_layout_from_struct_for_writeout(
                        env,
                        &writeout_vm,
                        &ty_linkage,
                        to_move_struct_tag_of(&object_type),
                    )?;
                    writeout_types.push((object_type, ty, layout));
                    let Some((_, ty, layout)) = writeout_types.last() else {
                        invariant_violation!("just pushed");
                    };
                    (*ty, layout)
                }
            };
            let abilities = ty.abilities();
            let has_public_transfer = abilities.has_store();
            let Some(bytes) = value.typed_serialize(layout) else {
                invariant_violation!("Failed to serialize already deserialized Move value");
            };
            // has_public_transfer has been determined by the abilities
            let move_object = create_written_object::<Mode>(
                env,
                &loaded_runtime_objects,
                id,
                ty,
                has_public_transfer,
                &bytes,
            )?;
            let object = Object::new_move(move_object, recipient, tx_digest);
            written_objects.insert(id, object);
        }

        for package in env.linkable_store.package_store.to_new_packages(bump) {
            let package_obj = Object::new_from_package(package, tx_digest);
            let id = package_obj.id();
            created_object_ids.insert(id);
            written_objects.insert(id, package_obj);
        }

        finish(
            bump,
            env.protocol_config,
            &*env.state_view,
            gas_charger,
            tx_context,
            &by_value_shared_objects,
            &consensus_owner_objects,
            loaded_runtime_objects,
            written_objects,
            created_object_ids,
            deleted_object_ids,
            user_events,
            accumulator_events,
            settlement_input_sui,
            settlement_output_sui,
        )
    }

    pub fn take_user_events(
        &mut self,
        vm: &MoveVM<'_>,
        version_mid: ModuleId<'a>,
        function_def_idx: FunctionDefinitionIndex,
        instr_length: u16,
        linkage: &ExecutableLinkage<'_>,
    ) -> Result<(), ExecutionError<'a>> {
        let events = object_runtime_mut!(self)?.take_user_events();
        let Some(num_events) = self.user_events.len().checked_add(events.len()) else {
            invariant_violation!("usize overflow, too many events emitted")
        };
        let max_events = self.env.protocol_config.max_num_event_emit();
        if num_events as u64 > max_events {
            let err = max_event_error(max_events)
                .at_code_offset(function_def_idx, instr_length)
                .finish(Location::Module(version_mid.to_move()));
            return Err(self.env.convert_linked_vm_error(err, linkage));
        }
        let mut new_events = Vec::with_capacity_in(events.len(), self.env.bump);
        // The layout of each event type, which the reference computes for every event. A hit is
        // never false: the layout is a deterministic function of the struct tag in `vm`.
        let mut layouts: Vec<
            '_,
            (
                StructTag<'a>,
                move_core_types::runtime_value::MoveTypeLayout,
            ),
        > = Vec::new_in(self.env.bump);
        for (tag, value) in events {
            let layout = match layouts.iter().find(|(t, _)| *t == tag) {
                Some((_, layout)) => {
                    debug_assert!(
                        vm.runtime_type_layout(&move_tags::TypeTag::Struct(Box::new(
                            to_move_struct_tag(&tag)
                        )))
                        .is_ok_and(|l| object_runtime::runtime_layouts_equal(&l, layout))
                    );
                    layout
                }
                None => {
                    let type_tag = move_tags::TypeTag::Struct(Box::new(to_move_struct_tag(&tag)));
                    let layout = vm
                        .runtime_type_layout(&type_tag)
                        .map_err(|e| self.env.convert_linked_vm_error(e, linkage))?;
                    layouts.push((tag, layout));
                    let Some((_, layout)) = layouts.last() else {
                        invariant_violation!("just pushed");
                    };
                    layout
                }
            };
            let Some(bytes) = value.typed_serialize(layout) else {
                invariant_violation!("Failed to serialize Move event");
            };
            new_events.push((version_mid, tag, alloc_slice_copy(self.env.bump, &bytes)));
        }
        self.user_events.extend(new_events);
        Ok(())
    }

    //
    // Final serialization of written objects
    //

    /// The writeout VM is used to serialize all written objects at the end of execution. This
    /// needs access to all types that were written during the transaction [`writes`]. Importantly,
    /// it needs to be able to create a VM over any newly published packages as the `init`
    /// functions in those packages may have created objects of types defined in those packages.
    fn make_writeout_vm<I>(
        env: &Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
        writes: I,
    ) -> Result<(MoveVM<'a>, ExecutableLinkage<'a>), ExecutionError<'a>>
    where
        I: IntoIterator<Item = MoveObjectType<'a>>,
    {
        let mut tys_addrs = BTreeSet::new_in(env.bump);
        tys_addrs.extend(
            writes
                .into_iter()
                .flat_map(|ty| move_object_type_all_addresses(env.bump, &ty))
                .map(|address| object_id(&address)),
        );

        let ty_linkage = ExecutableLinkage::type_linkage(
            *env.linkage_analysis.config(),
            tys_addrs.iter(),
            env.linkable_store,
        )?;
        env.vm
            .make_vm(
                &env.linkable_store.package_store,
                ty_linkage.linkage_context()?,
            )
            .map_err(|e| env.convert_linked_vm_error(e, &ty_linkage))
            .map(|vm| (vm, ty_linkage))
    }

    /// Load the type and layout for a struct tag.
    /// It is important that this use the VM passed in, and not the `resolution_vm` in the `env` as
    /// the types requested may have only been created during the execution of the transaction and
    /// therefore will not be present in the `resolution_vm`.
    fn load_type_and_layout_from_struct_for_writeout(
        env: &Env<'a, 'pc, 'vm, 'state, 'linkage, 'extension, Mode>,
        vm: &MoveVM,
        linkage: &ExecutableLinkage<'_>,
        tag: move_tags::StructTag,
    ) -> Result<(Type<'a>, move_core_types::runtime_value::MoveTypeLayout), ExecutionError<'a>>
    {
        let type_tag = move_tags::TypeTag::Struct(Box::new(tag));
        let vm_type = vm
            .load_type(&type_tag)
            .map_err(|e| env.convert_linked_vm_error(e, linkage))?;
        let layout = vm
            .runtime_type_layout(&type_tag)
            .map_err(|e| env.convert_vm_error(e))?;
        env.adapter_type_from_vm_type(vm, &vm_type)
            .map(|ty| (ty, layout))
    }

    //
    // Arguments and Values
    //

    fn location(
        &mut self,
        usage: UsageKind,
        location: T::Location,
    ) -> Result<Value, ExecutionError<'a>> {
        let resolved = self.locations.resolve(location)?;
        let mut local = match resolved {
            ResolvedLocation::Local(l) => l,
            ResolvedLocation::Pure {
                bytes,
                metadata,
                mut local,
            } => {
                if local.is_invalid()? {
                    let v = load_pure_value(self.gas_charger, self.env, bytes, metadata)?;
                    local.store(v)?;
                }
                local
            }
            ResolvedLocation::Receiving {
                metadata,
                mut local,
            } => {
                if local.is_invalid()? {
                    let v = load_receiving_value(self.gas_charger, self.env, metadata)?;
                    local.store(v)?;
                }
                local
            }
        };
        Ok(match usage {
            UsageKind::Move => {
                let value = local.move_()?;
                charge_gas_!(self.gas_charger, self.env, charge_move_loc, &value)?;
                value
            }
            UsageKind::Copy => {
                let value = local.copy()?;
                charge_gas_!(self.gas_charger, self.env, charge_copy_loc, &value)?;
                value
            }
            UsageKind::Borrow => {
                charge_gas_!(
                    self.gas_charger,
                    self.env,
                    charge_simple_instr(SimpleInstruction::MutBorrowLoc)
                )?;
                local.borrow()?
            }
        })
    }

    fn location_usage(&mut self, usage: T::Usage) -> Result<Value, ExecutionError<'a>> {
        match usage {
            T::Usage::Move(location) => self.location(UsageKind::Move, location),
            T::Usage::Copy { location, .. } => self.location(UsageKind::Copy, location),
        }
    }

    fn argument_value(
        &mut self,
        sp!(_, (arg_, _)): T::Argument<'a>,
    ) -> Result<Value, ExecutionError<'a>> {
        match arg_ {
            T::Argument__::Use(usage) => self.location_usage(usage),
            // freeze is a no-op for references since the value does not track mutability
            T::Argument__::Freeze(usage) => self.location_usage(usage),
            T::Argument__::Borrow(_, location) => self.location(UsageKind::Borrow, location),
            T::Argument__::Read(usage) => {
                let reference = self.location_usage(usage)?;
                charge_gas!(self, charge_read_ref, &reference)?;
                Ok(reference.read_ref()?)
            }
        }
    }

    pub fn argument<V>(&mut self, arg: T::Argument<'a>) -> Result<V, ExecutionError<'a>>
    where
        VMValue: VMValueCast<V>,
    {
        let before_height = self.gas_charger.move_gas_status().stack_height_current();
        let value = self.argument_value(arg)?;
        let after_height = self.gas_charger.move_gas_status().stack_height_current();
        debug_assert_eq!(before_height.saturating_add(1), after_height);
        let value: V = value.cast()?;
        Ok(value)
    }

    pub fn arguments<V>(
        &mut self,
        args: Vec<'a, T::Argument<'a>>,
    ) -> Result<Vec<'a, V>, ExecutionError<'a>>
    where
        VMValue: VMValueCast<V>,
    {
        let mut values = Vec::with_capacity_in(args.len(), self.env.bump);
        for arg in args {
            values.push(self.argument(arg)?);
        }
        Ok(values)
    }

    pub fn result(&mut self, result: Vec<'a, Option<CtxValue>>) -> Result<(), ExecutionError<'a>> {
        self.locations.results.push(Locals::new(
            self.env.bump,
            result.into_iter().map(|v| v.map(|v| v.0)),
        )?);
        Ok(())
    }

    pub fn charge_command(
        &mut self,
        is_move_call: bool,
        num_args: usize,
        num_return: usize,
    ) -> Result<(), ExecutionError<'a>> {
        let move_gas_status = self.gas_charger.move_gas_status_mut();
        let before_size = move_gas_status.stack_size_current();
        // Pop all of the arguments
        // If the return values came from the Move VM directly (via a Move call), pop those
        // as well
        let num_popped = if is_move_call {
            num_args.checked_add(num_return).ok_or_else(|| {
                make_invariant_violation!("usize overflow when charging gas for command",)
            })?
        } else {
            num_args
        };
        move_gas_status
            .charge(1, 0, num_popped as u64, 0, /* unused */ 1)
            .map_err(|e| self.env.convert_vm_error(e.finish(Location::Undefined)))?;
        let after_size = move_gas_status.stack_size_current();
        assert_invariant!(
            before_size == after_size,
            "We assume currently that the stack size is not decremented. \
            If this changes, we need to actually account for it here"
        );
        Ok(())
    }

    pub fn copy_value(&mut self, value: &CtxValue) -> Result<CtxValue, ExecutionError<'a>> {
        Ok(CtxValue(copy_value(self.gas_charger, self.env, &value.0)?))
    }

    pub fn new_coin(&mut self, amount: u64) -> Result<CtxValue, ExecutionError<'a>> {
        let id = self.tx_context.borrow_mut().fresh_id();
        object_runtime_mut!(self)?
            .new_id(id)
            .map_err(|e| self.env.convert_vm_error(e.finish(Location::Undefined)))?;
        Ok(CtxValue(Value::coin(id, amount)))
    }

    pub fn destroy_coin(&mut self, coin: CtxValue) -> Result<u64, ExecutionError<'a>> {
        let (id, amount) = coin.0.unpack_coin()?;
        object_runtime_mut!(self)?
            .delete_id(id)
            .map_err(|e| self.env.convert_vm_error(e.finish(Location::Undefined)))?;
        Ok(amount)
    }

    pub fn new_upgrade_cap(
        &mut self,
        version_id: ObjectId,
    ) -> Result<CtxValue, ExecutionError<'a>> {
        let id = self.tx_context.borrow_mut().fresh_id();
        object_runtime_mut!(self)?
            .new_id(id)
            .map_err(|e| self.env.convert_vm_error(e.finish(Location::Undefined)))?;
        let cap = UpgradeCap::new(id, version_id);
        Ok(CtxValue(Value::upgrade_cap(cap)))
    }

    pub fn upgrade_receipt(
        &self,
        upgrade_ticket: UpgradeTicket,
        upgraded_package_id: ObjectId,
    ) -> CtxValue {
        let receipt = UpgradeReceipt::new(upgrade_ticket, upgraded_package_id);
        CtxValue(Value::upgrade_receipt(receipt))
    }

    //
    // Move calls
    //

    pub fn vm_move_call(
        &mut self,
        function: T::LoadedFunction<'a>,
        args: Vec<'a, CtxValue>,
    ) -> Result<Vec<'a, CtxValue>, ExecutionError<'a>> {
        with_vm!(self, &function.linkage, |vm: &mut MoveVM<'a>| {
            let ty_args = function
                .type_arguments
                .iter()
                .map(|ty| {
                    let tag: move_tags::TypeTag = (*ty).try_into().map_err(|e| {
                        ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, e)
                    })?;
                    vm.load_type(&tag)
                        .map_err(|e| self.env.convert_linked_vm_error(e, &function.linkage))
                })
                .collect::<Result<std::vec::Vec<_>, ExecutionError<'a>>>()?;
            let max_type_nodes = accumulator_root::is_settle_u128_call(
                function.original_mid.address(),
                function.original_mid.name(),
                function.name,
            )
            .then_some(SETTLEMENT_MAX_TYPE_INSTANTIATION_NODES);
            let function_name = IdentStr::new(function.name).map_err(|e| {
                make_invariant_violation!("Invalid function name {}: {e}", function.name)
            })?;
            let result = self.execute_function_bypass_visibility_with_vm(
                vm,
                &function.original_mid.to_move(),
                function_name,
                ty_args,
                args,
                &function.linkage,
                max_type_nodes,
            )?;
            self.take_user_events(
                vm,
                function.version_mid,
                function.definition_index,
                function.instruction_length,
                &function.linkage,
            )?;
            Ok::<Vec<'a, CtxValue>, ExecutionError<'a>>(result)
        })
    }

    fn execute_function_bypass_visibility_with_vm(
        &mut self,
        vm: &mut MoveVM<'a>,
        original_mid: &move_tags::ModuleId,
        function_name: &IdentStr,
        ty_args: std::vec::Vec<VMType>,
        args: Vec<'a, CtxValue>,
        linkage: &ExecutableLinkage<'_>,
        max_type_nodes: Option<u64>,
    ) -> Result<Vec<'a, CtxValue>, ExecutionError<'a>> {
        let gas_status = self.gas_charger.move_gas_status_mut();
        let values = vm
            .execute_function_bypass_visibility_with_max_type_nodes(
                original_mid,
                function_name,
                ty_args,
                args.into_iter().map(|v| v.0.into()).collect(),
                &mut SuiGasMeter(gas_status),
                None,
                max_type_nodes,
            )
            .map_err(|e| self.env.convert_linked_vm_error(e, linkage))?;
        let mut results = Vec::with_capacity_in(values.len(), self.env.bump);
        results.extend(values.into_iter().map(|v| CtxValue(v.into())));
        Ok(results)
    }

    //
    // Publish and Upgrade
    //

    pub fn deserialize_package(
        &mut self,
        package_payload: PackagePayload<'a>,
        dep_ids: &[ObjectId],
    ) -> Result<DeserializedPackage<'a>, ExecutionError<'a>> {
        Ok(match package_payload {
            PackagePayload::Deserialized(deserialized_pkg) => deserialized_pkg,
            PackagePayload::Serialized(module_bytes) => {
                // This assertion is also checked in the call to `deserialize_modules`, but we
                // want to check it here first to keep existing behavior around checking this
                // invariant before the charge on pre-existing pathways.
                assert_invariant!(
                    !module_bytes.is_empty(),
                    "empty package is checked in transaction input checker"
                );
                let total_bytes = module_bytes.iter().map(|v| v.len()).sum();
                self.gas_charger.charge_publish_package(total_bytes)?;
                self.env.deserialize_package(module_bytes, dep_ids)?
            }
        })
    }

    fn fetch_package(
        &mut self,
        dependency_id: &ObjectId,
    ) -> Result<MovePackage<'a>, ExecutionError<'a>> {
        let fetched = self.fetch_packages(&[*dependency_id])?;
        let [fetched_package] = fetched.as_slice() else {
            invariant_violation!(
                "We should always fetch a single package for each object or return a dependency error."
            )
        };
        Ok(*fetched_package)
    }

    fn fetch_packages(
        &mut self,
        dependency_ids: &[ObjectId],
    ) -> Result<Vec<'a, MovePackage<'a>>, ExecutionError<'a>> {
        let bump = self.env.bump;
        let mut fetched = Vec::with_capacity_in(dependency_ids.len(), bump);
        let mut missing = Vec::new_in(bump);

        // Collect into a set to avoid duplicate fetches and preserve existing behavior
        let mut dependency_id_set: BTreeSet<&ObjectId> = BTreeSet::new_in(bump);
        dependency_id_set.extend(dependency_ids.iter());

        for id in &dependency_id_set {
            match self.env.linkable_store.get_move_package(id) {
                Err(e) => {
                    return Err(ExecutionError::new_with_source(
                        ExecutionErrorKind::PublishUpgradeMissingDependency,
                        e,
                    ));
                }
                Ok(Some(inner)) => {
                    fetched.push(inner);
                }
                Ok(None) => {
                    missing.push(**id);
                }
            }
        }

        if missing.is_empty() {
            assert_invariant!(
                fetched.len() == dependency_id_set.len(),
                "all dependencies should be fetched"
            );
            Ok(fetched)
        } else {
            let msg = format!(
                "Missing dependencies: {}",
                missing
                    .into_iter()
                    .map(|dep| format!("{}", dep))
                    .collect::<std::vec::Vec<_>>()
                    .join(", ")
            );
            Err(ExecutionError::new_with_source(
                ExecutionErrorKind::PublishUpgradeMissingDependency,
                msg,
            ))
        }
    }

    fn publish_and_verify_modules(
        &mut self,
        package_id: ObjectId,
        pkg: &MovePackage<'a>,
        modules: &[CompiledModule],
        linkage: &ExecutableLinkage<'_>,
    ) -> Result<(VerifiedPackage, MoveVM<'a>), ExecutionError<'a>> {
        let serialized_pkg = into_serialized_move_package(pkg).map_err(|e| {
            make_invariant_violation!("Failed to serialize package for verification: {}", e)
        })?;
        let data_store = &self.env.linkable_store.package_store;
        let vm = self
            .env
            .vm
            .validate_package(
                data_store,
                move_address(&package_id),
                serialized_pkg,
                &mut SuiGasMeter(self.gas_charger.move_gas_status_mut()),
                self.native_extensions.clone(),
            )
            .map_err(|e| self.env.convert_linked_vm_error(e, linkage))?;

        // run the Sui verifier
        for module in modules {
            // Run Sui bytecode verifier, which runs some additional checks that assume the Move
            // bytecode verifier has passed.
            sui_verifier::verifier::sui_verify_module_unmetered(
                module,
                &std::collections::BTreeMap::new(),
                &self
                    .env
                    .protocol_config
                    .verifier_config(/* signing_limits */ None),
            )
            .map_err(convert_sui_verifier_error)?;
        }

        Ok(vm)
    }

    // Here we optimistically push the package that is being published/upgraded
    // and if there is an error of any kind (verification or module init) we
    // remove it.
    // The call to `pop_last_package` later is fine because we cannot re-enter and
    // the last package we pushed is the one we are verifying and running the init from
    fn push_package_and_init_selected_modules<'m>(
        &mut self,
        package_id: ObjectId,
        package: MovePackage<'a>,
        verified_pkg: VerifiedPackage,
        vm: MoveVM<'a>,
        modules: impl IntoIterator<Item = &'m CompiledModule>,
        expected_inits: BTreeSet<'a, &'a str>,
        linkage: &ExecutableLinkage<'_>,
        expected_stack_height: u64,
    ) -> Result<(), ExecutionError<'a>> {
        self.env
            .linkable_store
            .package_store
            .push_package(package_id, package, verified_pkg)?;

        match self.init_selected_modules(
            vm,
            package_id,
            modules,
            expected_inits,
            linkage,
            expected_stack_height,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.env
                    .linkable_store
                    .package_store
                    .pop_package(package_id)?;
                Err(e)
            }
        }
    }

    fn init_selected_modules<'m>(
        &mut self,
        mut vm: MoveVM<'a>,
        package_id: ObjectId,
        modules: impl IntoIterator<Item = &'m CompiledModule>,
        mut expected_inits: BTreeSet<'a, &'a str>,
        linkage: &ExecutableLinkage<'_>,
        expected_stack_height: u64,
    ) -> Result<(), ExecutionError<'a>> {
        debug_assert_eq!(
            self.gas_charger.move_gas_status().stack_height_current(),
            expected_stack_height,
        );
        let check_expected_inits = self.env.protocol_config.harden_linkage_consistency();
        for module in modules {
            let Some((fdef_idx, fdef)) = module.find_function_def_by_name(INIT_FN_NAME.as_str())
            else {
                continue;
            };
            let module_name = module.identifier_at(module.self_handle().name);
            let fhandle = module.function_handle_at(fdef.function);
            let fparameters = module.signature_at(fhandle.parameters);
            assert_invariant!(
                fparameters.0.len() <= 2,
                "init function should have at most 2 parameters"
            );
            let has_otw = fparameters.0.len() == 2;
            let tx_context = self
                .location(UsageKind::Borrow, T::Location::TxContext)
                .map_err(|e| {
                    make_invariant_violation!("Failed to get tx context for init function: {}", e)
                })?;
            // balance the stack after borrowing the tx context
            charge_gas!(self, charge_store_loc, &tx_context)?;

            if check_expected_inits {
                assert_invariant!(
                    expected_inits.remove(module_name.as_str()),
                    "module {module_name} defines an `init` but was not recorded as doing so when \
                     the package payload was deserialized"
                );
            }

            let mut args = Vec::with_capacity_in(2, self.env.bump);
            if has_otw {
                args.push(CtxValue(Value::one_time_witness()?));
            }
            args.push(CtxValue(tx_context));
            debug_assert_eq!(
                self.gas_charger.move_gas_status().stack_height_current(),
                expected_stack_height,
            );
            let return_values = self.execute_function_bypass_visibility_with_vm(
                &mut vm,
                &module.self_id(),
                INIT_FN_NAME,
                vec![],
                args,
                linkage,
                None,
            )?;

            // The reference takes the name from `module.self_id()` again.
            let version_mid = ModuleId {
                address: move_address(&package_id),
                name: alloc_str(self.env.bump, module_name.as_str()),
            };
            self.take_user_events(
                &vm,
                version_mid,
                fdef_idx,
                fdef.code
                    .as_ref()
                    .map(|c| checked_as!(c.code.len(), u16))
                    .transpose()?
                    .unwrap_or(0),
                linkage,
            )?;
            assert_invariant!(
                return_values.is_empty(),
                "init should not have return values"
            );
            debug_assert_eq!(
                self.gas_charger.move_gas_status().stack_height_current(),
                expected_stack_height,
            );
        }

        // Every module we expected to initialize from earlier was initialized.
        assert_invariant!(
            !check_expected_inits || expected_inits.is_empty(),
            "modules {expected_inits:?} define an `init` that was never run"
        );

        Ok(())
    }

    pub fn publish_and_init_package(
        &mut self,
        package_payload: DeserializedPackage<'a>,
        dep_ids: &[ObjectId],
        linkage: ResolvedLinkage<'a>,
    ) -> Result<ObjectId, ExecutionError<'a>> {
        let bump = self.env.bump;
        let DeserializedPackage {
            deserialized_modules: mut modules,
            modules_with_init,
            ..
        } = package_payload;
        let original_id = if Mode::packages_are_predefined() {
            // do not calculate or substitute id for predefined packages
            object_id(modules.safe_get(0)?.self_id().address())
        } else {
            // It should be fine that this does not go through the object runtime since it does not
            // need to know about new packages created, since Move objects and Move packages
            // cannot interact
            let id = self.tx_context.borrow_mut().fresh_id();
            adapter::substitute_package_id(&mut modules, id)?;
            id
        };

        let dependencies = self.fetch_packages(dep_ids)?;
        let package = move_package::new_initial(
            bump,
            &modules,
            self.env.protocol_config,
            dependencies.iter(),
        )?;
        let package_id = *package.id;

        let linkage =
            ResolvedLinkage::update_for_publication(bump, package_id, original_id, linkage);

        let (pkg, vm) =
            self.publish_and_verify_modules(original_id, &package, &modules, &linkage)?;
        self.push_package_and_init_selected_modules(
            package_id,
            package,
            pkg,
            vm,
            &modules,
            modules_with_init,
            &linkage,
            PUBLISH_INIT_EXPECTED_STACK_HEIGHT,
        )?;
        Ok(original_id)
    }

    pub fn upgrade(
        &mut self,
        package_payload: DeserializedPackage<'a>,
        dep_ids: &[ObjectId],
        current_package_id: ObjectId,
        upgrade_ticket_policy: u8,
        linkage: ResolvedLinkage<'a>,
    ) -> Result<ObjectId, ExecutionError<'a>> {
        let bump = self.env.bump;
        let DeserializedPackage {
            deserialized_modules: mut modules,
            modules_with_init,
            ..
        } = package_payload;
        // Check that this package ID points to a package and get the package we're upgrading.
        let current_move_package = self.fetch_package(&current_package_id)?;

        let original_id = original_package_id(&current_move_package);
        adapter::substitute_package_id(&mut modules, original_id)?;

        // Upgraded packages share their predecessor's runtime ID but get a new storage ID.
        // It should be fine that this does not go through the object runtime since it does not
        // need to know about new packages created, since Move objects and Move packages
        // cannot interact
        let version_id = self.tx_context.borrow_mut().fresh_id();

        let dependencies = self.fetch_packages(dep_ids)?;
        let package = move_package::new_upgraded(
            bump,
            &current_move_package,
            version_id,
            &modules,
            self.env.protocol_config,
            dependencies.iter(),
        )?;

        let linkage =
            ResolvedLinkage::update_for_publication(bump, version_id, original_id, linkage);
        let (verified_pkg, vm) =
            self.publish_and_verify_modules(original_id, &package, &modules, &linkage)?;

        check_compatibility(
            bump,
            self.env.protocol_config,
            &current_move_package,
            &modules,
            upgrade_ticket_policy,
        )?;

        // Find newly added modules to the package. Only these modules are eligible for init on
        // upgrade; existing modules must not have init called even if they newly add one.
        let mut current_module_names: BTreeSet<&str> = BTreeSet::new_in(bump);
        current_module_names.extend(
            current_move_package
                .module_map
                .iter()
                .map(|(name, _)| *name),
        );
        let mut new_modules: Vec<&CompiledModule> = Vec::new_in(bump);
        new_modules.extend(modules.iter().filter(|m| {
            let name = m.identifier_at(m.self_handle().name).as_str();
            !current_module_names.contains(name)
        }));

        if self.env.protocol_config.enable_init_on_upgrade() {
            // Only newly added modules have their `init` functions called on upgrade.
            let mut expected_inits: BTreeSet<'a, &'a str> = BTreeSet::new_in(bump);
            expected_inits.extend(
                modules_with_init
                    .into_iter()
                    .filter(|name| !current_module_names.contains(name)),
            );
            self.push_package_and_init_selected_modules(
                version_id,
                package,
                verified_pkg,
                vm,
                new_modules.iter().copied(),
                expected_inits,
                &linkage,
                UPGRADE_INIT_EXPECTED_STACK_HEIGHT,
            )?;
        } else {
            let new_module_has_init = new_modules.iter().any(|module| {
                module.function_defs.iter().any(|fdef| {
                    let fhandle = module.function_handle_at(fdef.function);
                    let fname = module.identifier_at(fhandle.name);
                    fname == INIT_FN_NAME
                })
            });
            if new_module_has_init {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::FeatureNotYetSupported,
                    "`init` in new modules on upgrade is not yet supported",
                ));
            }

            self.env.linkable_store.package_store.push_package(
                version_id,
                package,
                verified_pkg,
            )?;
        }

        Ok(version_id)
    }

    //
    // Commands
    //

    pub fn transfer_object(
        &mut self,
        recipient: Owner<'a>,
        ty: Type<'a>,
        object: CtxValue,
    ) -> Result<(), ExecutionError<'a>> {
        self.transfer_object_(recipient, ty, object, /* end of transaction */ false)
    }

    fn transfer_object_(
        &mut self,
        recipient: Owner<'a>,
        ty: Type<'a>,
        object: CtxValue,
        end_of_transaction: bool,
    ) -> Result<(), ExecutionError<'a>> {
        // The reference converts to an owned `TypeTag`; the type is recorded as a view instead.
        let tag = ty
            .type_tag_in(self.env.bump)
            .map_err(|_| make_invariant_violation!("Unable to convert Type to TypeTag"))?;
        let TypeTag::Struct(tag) = tag else {
            invariant_violation!("Expected struct type tag");
        };
        let ty = move_object_type_of(&tag);
        object_runtime_mut!(self)?
            .transfer(recipient, ty, object.0.into(), end_of_transaction)
            .map_err(|e| self.env.convert_vm_error(e.finish(Location::Undefined)))?;
        Ok(())
    }

    //
    // Dev Inspect tracking
    //

    #[allow(clippy::type_complexity)]
    pub fn argument_updates(
        &mut self,
        args: Vec<'a, T::Argument<'a>>,
    ) -> Result<Vec<'a, (messages::transaction::Argument, &'a [u8], TypeTag<'a>)>, ExecutionError<'a>>
    {
        let mut updates = Vec::with_capacity_in(args.len(), self.env.bump);
        for arg in args {
            if let Some(update) = self.argument_update(arg)? {
                updates.push(update);
            }
        }
        Ok(updates)
    }

    #[allow(clippy::type_complexity)]
    fn argument_update(
        &mut self,
        sp!(_, (arg, ty)): T::Argument<'a>,
    ) -> Result<Option<(messages::transaction::Argument, &'a [u8], TypeTag<'a>)>, ExecutionError<'a>>
    {
        use messages::transaction::Argument as TxArgument;
        let ty = match ty {
            Type::Reference(true, inner) => *inner,
            ty => {
                debug_assert!(
                    false,
                    "Unexpected non reference type in location update: {ty:?}"
                );
                return Ok(None);
            }
        };
        let Ok(tag) = ty.type_tag_in(self.env.bump) else {
            invariant_violation!("unable to generate type tag from type")
        };
        let location = arg.location();
        let resolved = self.locations.resolve(location)?;
        let local = match resolved {
            ResolvedLocation::Local(local)
            | ResolvedLocation::Pure { local, .. }
            | ResolvedLocation::Receiving { local, .. } => local,
        };
        if local.is_invalid()? {
            return Ok(None);
        }
        // copy the value from the local
        let value = local.copy()?;
        let value = match arg {
            T::Argument__::Use(_) => {
                // dereference the reference
                value.read_ref()?
            }
            T::Argument__::Borrow(_, _) => {
                // value is not a reference, nothing to do
                value
            }
            T::Argument__::Freeze(_) => {
                invariant_violation!("freeze should not be used for a mutable reference")
            }
            T::Argument__::Read(_) => {
                invariant_violation!("read should not return a reference")
            }
        };
        let layout = self.env.runtime_layout(&ty)?;
        let Some(bytes) = value.typed_serialize(&layout) else {
            invariant_violation!("Failed to serialize Move value");
        };
        let arg = match location {
            T::Location::TxContext => return Ok(None),
            T::Location::GasCoin => TxArgument::GasCoin,
            T::Location::Result(i, j) => TxArgument::NestedResult(i, j),
            T::Location::ObjectInput(i) => TxArgument::Input(
                self.locations
                    .input_object_metadata
                    .safe_get(i as usize)?
                    .0
                    .0,
            ),
            T::Location::WithdrawalInput(i) => TxArgument::Input(
                self.locations
                    .input_withdrawal_metadata
                    .safe_get(i as usize)?
                    .original_input_index
                    .0,
            ),
            T::Location::PureInput(i) => TxArgument::Input(
                self.locations
                    .pure_input_metadata
                    .safe_get(i as usize)?
                    .original_input_index
                    .0,
            ),
            T::Location::ReceivingInput(i) => TxArgument::Input(
                self.locations
                    .receiving_input_metadata
                    .safe_get(i as usize)?
                    .original_input_index
                    .0,
            ),
        };
        Ok(Some((arg, alloc_slice_copy(self.env.bump, &bytes), tag)))
    }
}

impl VMValueCast<CtxValue> for VMValue {
    fn cast(self) -> Result<CtxValue, PartialVMError> {
        Ok(CtxValue(self.into()))
    }
}

impl CtxValue {
    pub fn vec_pack<'a>(
        ty: Type<'a>,
        values: Vec<'a, CtxValue>,
    ) -> Result<CtxValue, ExecutionError<'a>> {
        Ok(CtxValue(Value::vec_pack(
            ty,
            values.into_iter().map(|v| v.0),
        )?))
    }

    pub fn coin_ref_value(self) -> Result<u64, ExecutionError<'static>> {
        self.0.coin_ref_value()
    }

    pub fn coin_ref_subtract_balance(self, amount: u64) -> Result<(), ExecutionError<'static>> {
        self.0.coin_ref_subtract_balance(amount)
    }

    pub fn coin_ref_add_balance(self, amount: u64) -> Result<(), ExecutionError<'static>> {
        self.0.coin_ref_add_balance(amount)
    }

    pub fn into_upgrade_ticket(self) -> Result<UpgradeTicket, ExecutionError<'static>> {
        self.0.into_upgrade_ticket()
    }

    pub fn to_address(&self) -> Result<AccountAddress, ExecutionError<'static>> {
        self.0.copy()?.cast()
    }
}

/// The layouts of the input objects' types loaded so far, by adapter type, within `Context::new`.
///
/// A hit is never false: equal `Type`s convert to equal type tags, and both layouts are a
/// deterministic function of the tag, the package store and the input type resolution VM, none of
/// which change while the inputs are loaded. Only layouts computed without error are kept.
struct InputLayouts<'a>(
    Vec<
        'a,
        (
            Type<'a>,
            move_core_types::annotated_value::MoveTypeLayout,
            move_core_types::runtime_value::MoveTypeLayout,
        ),
    >,
);

fn load_object_arg<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    input_object_map: &mut BTreeMap<'a, ObjectId, object_runtime::InputObject<'a>>,
    input_layouts: &mut InputLayouts<'a>,
    input: T::ObjectInput<'a>,
) -> Result<(T::InputIndex, InputObjectMetadata<'a>, Value), ExecutionError<'a>> {
    let id = input.arg.id();
    let refined_permissions = input.arg.refined_permissions;
    let (metadata, value) = load_object_arg_impl(
        meter,
        env,
        input_object_map,
        input_layouts,
        id,
        refined_permissions,
        input.ty,
    )?;
    Ok((input.original_input_index, metadata, value))
}

fn load_object_arg_impl<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    input_object_map: &mut BTreeMap<'a, ObjectId, object_runtime::InputObject<'a>>,
    input_layouts: &mut InputLayouts<'a>,
    id: ObjectId,
    refined_permissions: ObjectPermissions,
    ty: T::Type<'a>,
) -> Result<(InputObjectMetadata<'a>, Value), ExecutionError<'a>> {
    let obj = env.read_object(&id)?;
    let owner = *obj.owner();
    let version = obj.version();
    let object_metadata = InputObjectMetadata {
        newly_created: false,
        id,
        refined_permissions,
        owner,
        version,
        type_: ty,
    };
    let Some(move_obj) = obj.try_as_move() else {
        invariant_violation!("Expected a Move object");
    };
    assert_expected_move_object_type(env.bump, &object_metadata.type_, &move_obj.type_)?;
    // The reference computes both layouts for every object; a repeated type reuses them (see
    // `InputLayouts`), in the same order of computations as the first time.
    let cached = input_layouts.0.iter().find(|(t, _, _)| *t == ty);
    if let Some((_, annotated, runtime)) = cached {
        debug_assert!(env.fully_annotated_layout(&ty).ok().as_ref() == Some(annotated));
        debug_assert!(
            env.runtime_layout(&ty)
                .is_ok_and(|l| object_runtime::runtime_layouts_equal(&l, runtime))
        );
    }
    let computed_annotated = match cached {
        Some(_) => None,
        None => Some(env.fully_annotated_layout(&ty)?),
    };
    let fully_annotated_layout = match (cached, &computed_annotated) {
        (Some((_, annotated, _)), _) => annotated,
        (None, Some(annotated)) => annotated,
        (None, None) => invariant_violation!("layout neither cached nor computed"),
    };
    let contained_uids = get_all_uids(env.bump, fully_annotated_layout, move_obj.contents)
        .map_err(|e| {
            make_invariant_violation!("Unable to retrieve UIDs for object. Got error: {e}")
        })?;
    input_object_map.insert(
        id,
        object_runtime::InputObject {
            contained_uids,
            version,
            owner,
        },
    );

    let v = match (cached, computed_annotated) {
        (Some((_, _, runtime)), _) => {
            Value::deserialize_with_layout(move_obj.contents, ty, runtime)?
        }
        (None, Some(annotated)) => {
            let runtime = env.runtime_layout(&ty)?;
            let v = Value::deserialize_with_layout(move_obj.contents, ty, &runtime)?;
            input_layouts.0.push((ty, annotated, runtime));
            v
        }
        (None, None) => invariant_violation!("layout neither cached nor computed"),
    };
    charge_gas_!(meter, env, charge_copy_loc, &v)?;
    charge_gas_!(meter, env, charge_store_loc, &v)?;
    Ok((object_metadata, v))
}

fn load_withdrawal_arg<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    withdrawal: &T::WithdrawalInput<'a>,
) -> Result<Value, ExecutionError<'a>> {
    let T::WithdrawalInput {
        original_input_index: _,
        ty: _,
        source,
        amount,
    } = withdrawal;
    let loaded = match source {
        T::WithdrawalSource::Direct { owner } => {
            Value::funds_accumulator_withdrawal(*owner, *amount)
        }
        T::WithdrawalSource::Allowance { funder, id } => {
            // Leaves room for a future `SponsorAllowance` with `is_sponsor` set
            Value::allowance_withdrawal(*id, *funder, *amount, /* is_sponsor */ false)
        }
    };
    charge_gas_!(meter, env, charge_copy_loc, &loaded)?;
    charge_gas_!(meter, env, charge_store_loc, &loaded)?;
    Ok(loaded)
}

fn load_pure_value<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    bytes: &[u8],
    metadata: &T::PureInput<'a>,
) -> Result<Value, ExecutionError<'a>> {
    let loaded = Value::deserialize(env, bytes, metadata.ty)?;
    // ByteValue::Receiving { id, version } => Value::receiving(*id, *version),
    charge_gas_!(meter, env, charge_copy_loc, &loaded)?;
    charge_gas_!(meter, env, charge_store_loc, &loaded)?;
    Ok(loaded)
}

fn load_receiving_value<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    metadata: &T::ReceivingInput<'a>,
) -> Result<Value, ExecutionError<'a>> {
    let (id, version, _) = metadata.object_ref;
    let loaded = Value::receiving(id, version);
    charge_gas_!(meter, env, charge_copy_loc, &loaded)?;
    charge_gas_!(meter, env, charge_store_loc, &loaded)?;
    Ok(loaded)
}

fn copy_value<'a, Mode: ExecutionMode>(
    meter: &mut GasCharger<'a>,
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    value: &Value,
) -> Result<Value, ExecutionError<'a>> {
    charge_gas_!(meter, env, charge_copy_loc, value)?;
    charge_gas_!(meter, env, charge_pop, value)?;
    value.copy()
}

/// The max budget was deducted from the gas coin at the beginning of the transaction,
/// now we return exactly that amount. Gas will be charged by the execution engine.
/// If the gas coin was transferred in any way, set the charge location.
fn refund_max_gas_budget<'a, OType>(
    bump: &'a Bump,
    writes: &mut IndexMap<'a, ObjectId, (Owner<'a>, OType, VMValue)>,
    accumulator_events: &mut Vec<'a, MoveAccumulatorEvent<'a>>,
    gas_charger: &mut GasCharger<'a>,
    gas_id: ObjectId,
    gas_coin_transfer: Option<&GasCoinTransfer>,
) -> Result<(), ExecutionError<'a>> {
    match gas_coin_transfer {
        Some(GasCoinTransfer::SendFunds { recipient, .. }) => {
            // if the gas coin was transferred to an address balance, send the budget to that
            // address balance
            assert_invariant!(
                !writes.contains_key(&gas_id),
                "Gas coin should not be in writes if it was used with send_funds"
            );
            balance_change_accumulator_event(
                bump,
                accumulator_events,
                *recipient,
                checked_as!(gas_charger.gas_budget(), i64)?,
            )?;
        }
        Some(GasCoinTransfer::TransferObjects) | None => {
            let Some((_, _, value_ref)) = writes.get_mut(&gas_id) else {
                invariant_violation!("Gas object cannot be wrapped or destroyed")
            };
            // replace with dummy value
            let value = std::mem::replace(value_ref, VMValue::u8(0));
            let mut locals = Locals::new(bump, [Some(value.into())])?;
            let mut local = locals.local(0)?;
            let coin_value = local.borrow()?.coin_ref_value()?;
            let additional = gas_charger.gas_budget();
            if coin_value.checked_add(additional).is_none() {
                return Err(ExecutionError::new_with_source(
                    ExecutionErrorKind::CoinBalanceOverflow,
                    "Gas coin too large after returning the max gas budget",
                ));
            };
            local.borrow()?.coin_ref_add_balance(additional)?;
            // put the value back
            *value_ref = local.move_()?.into();
        }
    };
    Ok(())
}

/// Refunds the gas budget, overrides the charge location if transferred, and for ephemeral
/// coins settles the net balance change back to the source address balance.
/// Writes are always updated for refunding the gas budget.
/// The ephemeral coin is removed from writes and created objects in the case that it was not
/// transferred (left in its memory location at the end of the transaction).
fn finish_gas_coin<'a, OType>(
    bump: &'a Bump,
    gas_charger: &mut GasCharger<'a>,
    writes: &mut IndexMap<'a, ObjectId, (Owner<'a>, OType, VMValue)>,
    created_object_ids: &mut IndexSet<'a, ObjectId>,
    deleted_object_ids: &IndexSet<'a, ObjectId>,
    accumulator_events: &mut Vec<'a, MoveAccumulatorEvent<'a>>,
    gas_id: ObjectId,
    gas_payment: GasPayment,
    gas_coin_transfer: Option<GasCoinTransfer>,
) -> Result<(), ExecutionError<'a>> {
    // return the max gas budget to the current gas location
    refund_max_gas_budget(
        bump,
        writes,
        accumulator_events,
        gas_charger,
        gas_id,
        gas_coin_transfer.as_ref(),
    )?;

    // Set the charge location if it was transferred
    // This might not actually "override" (that is the actual charge location might be the same
    // as it was at the beginning of the transaction), in the case where the coin was real and
    // was transferred, or in the case where the coin was ephemeral and transferred to the
    // an address balance recipient that matches the original address balance charge location
    match &gas_coin_transfer {
        Some(GasCoinTransfer::SendFunds { recipient, .. }) => {
            gas_charger.override_gas_charge_location(PaymentLocation::AddressBalance(
                SuiAddress(recipient.into_bytes()),
            ))?;
        }
        Some(GasCoinTransfer::TransferObjects) => {
            gas_charger.override_gas_charge_location(PaymentLocation::Coin(gas_id))?;
        }
        None => (),
    }

    // If the gas coin was not ephemeral, then we are done.
    let address = match gas_payment.location {
        PaymentLocation::Coin(_) => {
            // small sanity check
            assert_invariant!(
                !matches!(gas_coin_transfer, Some(GasCoinTransfer::SendFunds { .. }))
                    || deleted_object_ids.contains(&gas_id),
                "send_funds transfer implies the coin should be deleted"
            );
            return Ok(());
        }
        PaymentLocation::AddressBalance(address) => address,
    };

    let net_balance_change = if let Some(gas_coin_transfer) = gas_coin_transfer {
        // sanity check storage changes
        match gas_coin_transfer {
            GasCoinTransfer::TransferObjects => {
                assert_invariant!(
                    created_object_ids.contains(&gas_id),
                    "ephemeral coin should be newly created"
                );
                assert_invariant!(
                    !deleted_object_ids.contains(&gas_id),
                    "ephemeral coin should not be deleted if transferred as an object"
                );
                assert_invariant!(
                    writes.contains_key(&gas_id),
                    "ephemeral coin should be in writes if transferred as an object"
                );
            }
            GasCoinTransfer::SendFunds { .. } => {
                assert_invariant!(
                    !created_object_ids.contains(&gas_id),
                    "ephemeral coin should not be newly created if transferred with send_funds"
                );
                assert_invariant!(
                    !deleted_object_ids.contains(&gas_id),
                    "ephemeral coin should not be deleted if transferred with send_funds"
                );
                assert_invariant!(
                    !writes.contains_key(&gas_id),
                    "ephemeral coin should not be in writes if transferred with send_funds"
                );
            }
        }

        // If the gas coin was moved, it was transferred.
        // In such a case, the gas coin has a new location, so we fully withdraw the gas amount
        // and keep it in the coin object. The transferred location is now the source of payment
        // instead of the address balance.
        let Some(net_balance_change) = gas_payment
            .amount
            .try_into()
            .ok()
            .and_then(|i: i64| i.checked_neg())
        else {
            invariant_violation!("Gas payment amount cannot be represented as i64")
        };
        net_balance_change
    } else {
        // In this case the gas coin was not moved, so we want to return the remaining balance to
        // the address balance. To do so we need to destroy it and create an accumulator event for
        // the net balance change
        let was_created = created_object_ids.shift_remove(&gas_id);
        assert_invariant!(was_created, "ephemeral coin should be newly created");
        let Some((_owner, _ty, value)) = writes.shift_remove(&gas_id) else {
            invariant_violation!("checked above that the gas coin was present")
        };
        let (_id, remaining_balance) = Value::from(value).unpack_coin()?;
        // gas_payment.amount is the original value of the ephemeral coin.
        // If net_balance_change is negative, then balance was spent/withdrawn from the gas coin.
        // If the net_balance_change is positive, then balance was added/merged to the gas coin.
        let Some(net_balance_change): Option<i64> = (remaining_balance as i128)
            .checked_sub(gas_payment.amount as i128)
            .and_then(|i| i.try_into().ok())
        else {
            invariant_violation!("Remaining balance could not be represented as i64")
        };
        net_balance_change
    };
    balance_change_accumulator_event(
        bump,
        accumulator_events,
        AccountAddress::new(address.0),
        net_balance_change,
    )?;
    Ok(())
}

fn balance_change_accumulator_event<'a>(
    bump: &'a Bump,
    accumulator_events: &mut Vec<'a, MoveAccumulatorEvent<'a>>,
    address: AccountAddress,
    balance_change: i64,
) -> Result<(), ExecutionError<'a>> {
    if balance_change == 0 {
        return Ok(());
    }
    let balance_type = accumulator_root::sui_balance_type(bump);
    let Some(accumulator_id) =
        accumulator_root::get_field_id(&SuiAddress(address.into_bytes()), &balance_type).ok()
    else {
        invariant_violation!("Failed to compute accumulator field id")
    };
    let (action, value) = if balance_change < 0 {
        (
            MoveAccumulatorAction::Split,
            MoveAccumulatorValue::U64(balance_change.unsigned_abs()),
        )
    } else {
        (
            MoveAccumulatorAction::Merge,
            MoveAccumulatorValue::U64(balance_change as u64),
        )
    };
    accumulator_events.push(MoveAccumulatorEvent {
        accumulator_id,
        action,
        target_addr: address,
        target_ty: balance_type,
        value,
    });
    Ok(())
}

/// `MoveObject::new_from_execution`: the object, if it is within the size bound.
///
/// `has_public_transfer` must have been determined from the type's abilities, or propagated from
/// the inputs by the runtime (the reference marks the function `unsafe` for this).
fn new_move_object_from_execution<'a>(
    type_: MoveObjectType<'a>,
    has_public_transfer: bool,
    version: SequenceNumber,
    contents: &'a [u8],
    protocol_config: &ProtocolConfig,
    system_mutation: bool,
) -> Result<MoveObject<'a>, ExecutionError<'a>> {
    let bound = if protocol_config.allow_unbounded_system_objects() && system_mutation {
        // The reference reports this through `debug_fatal!`.
        debug_assert!(
            contents.len() as u64 <= protocol_config.max_move_object_size(),
            "System created object (ID = {:?}) of type {:?} and size {} exceeds normal max size {}",
            contents.get(..ObjectId::LENGTH),
            type_,
            contents.len(),
            protocol_config.max_move_object_size()
        );
        u64::MAX
    } else {
        protocol_config.max_move_object_size()
    };
    // `MoveObject::new_from_execution_with_limit`.
    // coins should always have public transfer, as they always should have store.
    // Thus, type_ == GasCoin::type_() ==> has_public_transfer
    // TODO: think this can be generalized to is_coin
    debug_assert!(!matches!(type_, MoveObjectType::GasCoin) || has_public_transfer);
    if contents.len() as u64 > bound {
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::MoveObjectTooBig {
                object_size: contents.len() as u64,
                max_object_size: bound,
            },
        ));
    }
    Ok(MoveObject {
        type_,
        has_public_transfer,
        version,
        contents,
    })
}

/// Generate an MoveObject given an updated/written object
///
/// This function assumes proper generation of has_public_transfer, either from the abilities of
/// the StructTag, or from the runtime correctly propagating from the inputs. (The reference marks
/// it `unsafe` for this.)
fn create_written_object<'a, Mode: ExecutionMode>(
    env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
    objects_modified_at: &BTreeMap<'a, ObjectId, LoadedRuntimeObject>,
    id: ObjectId,
    type_: Type<'a>,
    has_public_transfer: bool,
    contents: &[u8],
) -> Result<MoveObject<'a>, ExecutionError<'a>> {
    debug_assert_eq!(
        Some(&id.0[..]),
        contents.get(..ObjectId::LENGTH),
        "object contents should start with an id"
    );
    let old_obj_ver = objects_modified_at
        .get(&id)
        .map(|obj: &LoadedRuntimeObject| obj.version);

    let Ok(type_tag) = type_.type_tag_in(env.bump) else {
        invariant_violation!("unable to generate type tag from type")
    };

    let struct_tag = match type_tag {
        TypeTag::Struct(inner) => inner,
        _ => invariant_violation!("Non struct type for object"),
    };
    new_move_object_from_execution(
        move_object_type_of(&struct_tag),
        has_public_transfer,
        old_obj_ver.unwrap_or_default(),
        alloc_slice_copy(env.bump, contents),
        env.protocol_config,
        Mode::packages_are_predefined(),
    )
}

/// The Sui verifier's error, which is `sui_types`': its kind is `SuiMoveVerificationError`, or a
/// timeout, which execution never sees.
fn convert_sui_verifier_error<'a>(e: sui_types::error::ExecutionError) -> ExecutionError<'a> {
    use sui_types::execution_status::ExecutionErrorKind as SuiKind;
    let kind = match e.kind() {
        SuiKind::SuiMoveVerificationError => ExecutionErrorKind::SuiMoveVerificationError,
        SuiKind::SuiMoveVerificationTimedout => ExecutionErrorKind::SuiMoveVerificationTimedout,
        kind => return make_invariant_violation!("Unexpected Sui verifier error {kind:?}"),
    };
    match e.source() {
        Some(source) => ExecutionError::new_with_source(kind, source.to_string()),
        None => ExecutionError::from_kind(kind),
    }
}

pub fn subst_signature(
    signature: LoadedFunctionInformation,
    type_arguments: &[VMType],
) -> VMResult<LoadedFunctionInformation> {
    let LoadedFunctionInformation {
        parameters,
        return_,
        is_entry,
        is_native,
        visibility,
        index,
        instruction_count,
    } = signature;
    let parameters = parameters
        .into_iter()
        .map(|ty| ty.subst(type_arguments))
        .collect::<PartialVMResult<std::vec::Vec<_>>>()
        .map_err(|err| err.finish(Location::Undefined))?;
    let return_ = return_
        .into_iter()
        .map(|ty| ty.subst(type_arguments))
        .collect::<PartialVMResult<std::vec::Vec<_>>>()
        .map_err(|err| err.finish(Location::Undefined))?;
    Ok(LoadedFunctionInformation {
        parameters,
        return_,
        is_entry,
        is_native,
        visibility,
        index,
        instruction_count,
    })
}

pub enum EitherError<'a> {
    CommandArgument(CommandArgumentError),
    Execution(ExecutionError<'a>),
}

impl<'a> From<ExecutionError<'a>> for EitherError<'a> {
    fn from(e: ExecutionError<'a>) -> Self {
        EitherError::Execution(e)
    }
}

impl From<CommandArgumentError> for EitherError<'_> {
    fn from(e: CommandArgumentError) -> Self {
        EitherError::CommandArgument(e)
    }
}

impl<'a> EitherError<'a> {
    pub fn into_execution_error(self, command_index: usize) -> ExecutionError<'a> {
        match self {
            EitherError::CommandArgument(e) => command_argument_error(e, command_index),
            EitherError::Execution(e) => e,
        }
    }
}

/***************************************************************************************************
 * Special serialization formats
 **************************************************************************************************/

/// Special enum for values that need additional validation, in other words
/// There is validation to do on top of the BCS layout. Currently only needed for
/// strings
// The reference boxes the inner layouts; here they are in the transaction's arena.
#[derive(Debug, Clone, Copy)]
pub enum PrimitiveArgumentLayout<'a> {
    /// An option
    Option(&'a PrimitiveArgumentLayout<'a>),
    /// A vector
    Vector(&'a PrimitiveArgumentLayout<'a>),
    /// An ASCII encoded string
    Ascii,
    /// A UTF8 encoded string
    UTF8,
    // needed for Option validation
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
}

impl PrimitiveArgumentLayout<'_> {
    /// returns true iff all BCS compatible bytes are actually values for this type.
    /// For example, this function returns false for Option and Strings since they need additional
    /// validation.
    pub fn bcs_only(&self) -> bool {
        match self {
            // have additional restrictions past BCS
            PrimitiveArgumentLayout::Option(_)
            | PrimitiveArgumentLayout::Ascii
            | PrimitiveArgumentLayout::UTF8 => false,
            // Move primitives are BCS compatible and do not need additional validation
            PrimitiveArgumentLayout::Bool
            | PrimitiveArgumentLayout::U8
            | PrimitiveArgumentLayout::U16
            | PrimitiveArgumentLayout::U32
            | PrimitiveArgumentLayout::U64
            | PrimitiveArgumentLayout::U128
            | PrimitiveArgumentLayout::U256
            | PrimitiveArgumentLayout::Address => true,
            // vector only needs validation if it's inner type does
            PrimitiveArgumentLayout::Vector(inner) => inner.bcs_only(),
        }
    }
}

/// Checks the bytes against the `SpecialArgumentLayout` using `bcs`. It does not actually generate
/// the deserialized value, only walks the bytes. While not necessary if the layout does not contain
/// special arguments (e.g. Option or String) we check the BCS bytes for predictability
pub fn bcs_argument_validate<'a>(
    bytes: &[u8],
    idx: u16,
    layout: PrimitiveArgumentLayout<'_>,
) -> Result<(), ExecutionError<'a>> {
    bcs::from_bytes_seed(&layout, bytes).map_err(|_| {
        ExecutionError::new_with_source(
            ExecutionErrorKind::command_argument_error(CommandArgumentError::InvalidBCSBytes, idx),
            format!("Function expects {layout} but provided argument's value does not match",),
        )
    })
}

impl<'d> serde::de::DeserializeSeed<'d> for &PrimitiveArgumentLayout<'_> {
    type Value = ();
    fn deserialize<D: serde::de::Deserializer<'d>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        use serde::de::Error;
        match self {
            PrimitiveArgumentLayout::Ascii => {
                let s: &str = serde::Deserialize::deserialize(deserializer)?;
                if !s.is_ascii() {
                    Err(D::Error::custom("not an ascii string"))
                } else {
                    Ok(())
                }
            }
            PrimitiveArgumentLayout::UTF8 => {
                deserializer.deserialize_string(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::Option(layout) => {
                deserializer.deserialize_option(OptionElementVisitor(layout))
            }
            PrimitiveArgumentLayout::Vector(layout) => {
                deserializer.deserialize_seq(VectorElementVisitor(layout))
            }
            // primitive move value cases, which are hit to make sure the correct number of bytes
            // are removed for elements of an option/vector
            PrimitiveArgumentLayout::Bool => {
                deserializer.deserialize_bool(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U8 => {
                deserializer.deserialize_u8(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U16 => {
                deserializer.deserialize_u16(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U32 => {
                deserializer.deserialize_u32(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U64 => {
                deserializer.deserialize_u64(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U128 => {
                deserializer.deserialize_u128(serde::de::IgnoredAny)?;
                Ok(())
            }
            PrimitiveArgumentLayout::U256 => {
                U256::deserialize(deserializer)?;
                Ok(())
            }
            PrimitiveArgumentLayout::Address => {
                messages::build::base::SuiAddress::deserialize(deserializer)?;
                Ok(())
            }
        }
    }
}

struct VectorElementVisitor<'l, 'a>(&'l PrimitiveArgumentLayout<'a>);

impl<'d> serde::de::Visitor<'d> for VectorElementVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Vector")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'d>,
    {
        while seq.next_element_seed(self.0)?.is_some() {}
        Ok(())
    }
}

struct OptionElementVisitor<'l, 'a>(&'l PrimitiveArgumentLayout<'a>);

impl<'d> serde::de::Visitor<'d> for OptionElementVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Option")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'d>,
    {
        serde::de::DeserializeSeed::deserialize(self.0, deserializer)
    }
}

impl fmt::Display for PrimitiveArgumentLayout<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrimitiveArgumentLayout::Vector(inner) => {
                write!(f, "vector<{inner}>")
            }
            PrimitiveArgumentLayout::Option(inner) => {
                write!(f, "std::option::Option<{inner}>")
            }
            PrimitiveArgumentLayout::Ascii => {
                write!(f, "std::{}::{}", RESOLVED_ASCII_STR.1, RESOLVED_ASCII_STR.2)
            }
            PrimitiveArgumentLayout::UTF8 => {
                write!(f, "std::{}::{}", RESOLVED_UTF8_STR.1, RESOLVED_UTF8_STR.2)
            }
            PrimitiveArgumentLayout::Bool => write!(f, "bool"),
            PrimitiveArgumentLayout::U8 => write!(f, "u8"),
            PrimitiveArgumentLayout::U16 => write!(f, "u16"),
            PrimitiveArgumentLayout::U32 => write!(f, "u32"),
            PrimitiveArgumentLayout::U64 => write!(f, "u64"),
            PrimitiveArgumentLayout::U128 => write!(f, "u128"),
            PrimitiveArgumentLayout::U256 => write!(f, "u256"),
            PrimitiveArgumentLayout::Address => write!(f, "address"),
        }
    }
}

/// `MoveObjectType::coin_type_maybe`, in `bump`.
fn coin_type_maybe<'a>(bump: &'a Bump, ty: &MoveObjectType<'a>) -> Option<TypeTag<'a>> {
    match ty {
        MoveObjectType::GasCoin => Some(accumulator_root::sui_type(bump)),
        MoveObjectType::Coin(inner) => Some(*inner),
        MoveObjectType::StakedSui
        | MoveObjectType::SuiBalanceAccumulatorField
        | MoveObjectType::BalanceAccumulatorField(_)
        | MoveObjectType::Other(_) => None,
    }
}

pub fn finish<'a>(
    bump: &'a Bump,
    protocol_config: &ProtocolConfig,
    state_view: &dyn ExecutionState<'a>,
    gas_charger: &mut GasCharger<'a>,
    tx_context: &TxContext,
    by_value_shared_objects: &BTreeSet<'a, ObjectId>,
    consensus_owner_objects: &BTreeMap<'a, ObjectId, Owner<'a>>,
    loaded_runtime_objects: BTreeMap<'a, ObjectId, LoadedRuntimeObject>,
    written_objects: BTreeMap<'a, ObjectId, Object<'a>>,
    created_object_ids: IndexSet<'a, ObjectId>,
    deleted_object_ids: IndexSet<'a, ObjectId>,
    user_events: Vec<'a, (ModuleId<'a>, StructTag<'a>, &'a [u8])>,
    accumulator_events: Vec<'a, MoveAccumulatorEvent<'a>>,
    settlement_input_sui: u64,
    settlement_output_sui: u64,
) -> Result<ExecutionResultsV2<'a>, ExecutionError<'a>> {
    // Before finishing, ensure that any shared object taken by value by the transaction is either:
    // 1. Mutated (and still has a shared ownership); or
    // 2. Deleted.
    // Otherwise, the shared object operation is not allowed and we fail the transaction.
    for id in by_value_shared_objects {
        // If it's been written it must have been reshared so must still have an ownership
        // of `Shared`.
        if let Some(obj) = written_objects.get(id) {
            if !obj.is_shared() {
                return Err(ExecutionError::new(
                    ExecutionErrorKind::SharedObjectOperationNotAllowed,
                    Some(
                        format!(
                            "Shared object operation on {} not allowed: \
                                 cannot be frozen, transferred, or wrapped",
                            id
                        )
                        .into(),
                    ),
                ));
            }
        } else {
            // If it's not in the written objects, the object must have been deleted. Otherwise
            // it's an error.
            if !deleted_object_ids.contains(id) {
                return Err(ExecutionError::new(
                    ExecutionErrorKind::SharedObjectOperationNotAllowed,
                    Some(
                        format!(
                            "Shared object operation on {} not allowed: \
                             shared objects used by value must be re-shared if not deleted",
                            id
                        )
                        .into(),
                    ),
                ));
            }
        }
    }

    // Before finishing, enforce auth restrictions on consensus objects.
    for (id, original_owner) in consensus_owner_objects {
        let Owner::ConsensusAddressOwner { owner, .. } = original_owner else {
            panic!(
                "verified before adding to `consensus_owner_objects` that these are ConsensusAddressOwner"
            );
        };
        // Already verified in pre-execution checks that tx sender is the object owner.
        // Owner is allowed to do anything with the object.
        if tx_context.sender() != **owner {
            // The reference reports this through `debug_fatal!`.
            debug_assert!(
                false,
                "transaction with a singly owned input object where the tx sender is not the owner should never be executed"
            );
            return Err(ExecutionError::new(
                ExecutionErrorKind::SharedObjectOperationNotAllowed,
                Some(
                    format!(
                        "Shared object operation on {} not allowed: \
                         transaction with singly owned input object must be sent by the owner",
                        id
                    )
                    .into(),
                ),
            ));
        }
        // If an Owner type is implemented with support for more fine-grained authorization,
        // checks should be performed here. For example, transfers and wraps can be detected
        // by comparing `original_owner` with:
        // let new_owner = written_objects.get(&id).map(|obj| obj.owner);
        //
        // Deletions can be detected with:
        // let deleted = deleted_object_ids.contains(&id);
    }

    let sender = alloc(bump, tx_context.sender());
    let mut events = Vec::with_capacity_in(user_events.len(), bump);
    events.extend(
        user_events
            .into_iter()
            .map(|(module_id, tag, contents)| Event {
                package_id: alloc(bump, object_id(module_id.address())),
                transaction_module: module_id.name(),
                sender,
                type_: tag,
                contents,
            }),
    );
    let user_events = events;

    let mut receiving_funds_type_and_owners: BTreeMap<'a, TypeTag<'a>, BTreeSet<'a, SuiAddress>> =
        BTreeMap::new_in(bump);
    let mut accumulator_writes = Vec::with_capacity_in(accumulator_events.len(), bump);
    for accum_event in accumulator_events {
        if let Some(ty) = accumulator_root::maybe_get_balance_type_param(&accum_event.target_ty) {
            receiving_funds_type_and_owners
                .entry(ty)
                .or_insert_with(|| BTreeSet::new_in(bump))
                .insert(SuiAddress(accum_event.target_addr.into_bytes()));
        }
        let value = match accum_event.value {
            MoveAccumulatorValue::U64(amount) => AccumulatorValue::Integer(amount),
            MoveAccumulatorValue::EventRef(event_idx) => {
                let Some(event) = user_events.get(checked_as!(event_idx, usize)?) else {
                    invariant_violation!(
                        "Could not find authenticated event at index {}",
                        event_idx
                    );
                };
                let digest = messages::fast::event_digest(bump, event);
                AccumulatorValue::EventDigest(alloc_slice_copy(
                    bump,
                    &[EventCommitment {
                        index: U64Le::new(event_idx),
                        digest,
                    }],
                ))
            }
        };

        let write = AccumulatorWriteV1 {
            address: alloc(bump, SuiAddress(accum_event.target_addr.into_bytes())),
            ty: accum_event.target_ty,
            operation: accum_event.action.into_sui_accumulator_action(),
            value,
        };

        accumulator_writes.push(AccumulatorEvent::new(accum_event.accumulator_id, write));
    }
    let accumulator_events = accumulator_writes;

    // Deny-list v2 checks
    for object in written_objects.values() {
        let coin_type = object.type_().and_then(|ty| coin_type_maybe(bump, ty));
        let owner = get_owner_address(object.owner());
        if let (Some(ty), Some(owner)) = (coin_type, owner) {
            receiving_funds_type_and_owners
                .entry(ty)
                .or_insert_with(|| BTreeSet::new_in(bump))
                .insert(owner);
        }
    }
    let DenyListResult {
        result,
        num_non_gas_coin_owners,
    } = state_view.check_coin_deny_list(receiving_funds_type_and_owners);
    gas_charger.charge_coin_transfers(protocol_config, num_non_gas_coin_owners)?;
    result?;

    let mut created = BTreeSet::new_in(bump);
    created.extend(created_object_ids);
    let created_object_ids = created;
    let mut deleted = BTreeSet::new_in(bump);
    deleted.extend(deleted_object_ids);
    let deleted_object_ids = deleted;
    let mut modified_objects = BTreeSet::new_in(bump);
    modified_objects.extend(
        loaded_runtime_objects
            .into_iter()
            .filter_map(|(id, loaded)| loaded.is_modified.then_some(id)),
    );

    assert_invariant!(
        created_object_ids.is_disjoint(&deleted_object_ids),
        "Created and deleted object sets should be disjoint"
    );
    assert_invariant!(
        modified_objects.is_disjoint(&created_object_ids),
        "Modified and created object sets should be disjoint"
    );
    assert_invariant!(
        written_objects
            .keys()
            .all(|id| !deleted_object_ids.contains(id)),
        "Written objects should not be deleted"
    );
    Ok(ExecutionResultsV2 {
        written_objects,
        modified_objects,
        created_object_ids,
        deleted_object_ids,
        user_events,
        accumulator_events,
        settlement_input_sui,
        settlement_output_sui,
    })
}

pub fn fetch_package<'a>(
    bump: &'a Bump,
    state_view: &dyn BackingPackageStore<'a>,
    package_id: &ObjectId,
) -> Result<Object<'a>, ExecutionError<'a>> {
    let mut fetched_packages = fetch_packages(bump, state_view, [package_id])?;
    assert_invariant!(
        fetched_packages.len() == 1,
        "Number of fetched packages must match the number of package object IDs if successful."
    );
    match fetched_packages.pop() {
        Some(pkg) => Ok(pkg),
        None => invariant_violation!(
            "We should always fetch a package for each object or return a dependency error."
        ),
    }
}

pub fn fetch_packages<'ctx, 'a>(
    bump: &'a Bump,
    state_view: &dyn BackingPackageStore<'a>,
    package_ids: impl IntoIterator<Item = &'ctx ObjectId>,
) -> Result<Vec<'a, Object<'a>>, ExecutionError<'a>> {
    let mut package_id_set = BTreeSet::new_in(bump);
    package_id_set.extend(package_ids);
    match get_package_objects(bump, state_view, package_id_set) {
        Err(e) => Err(ExecutionError::new_with_source(
            ExecutionErrorKind::PublishUpgradeMissingDependency,
            e,
        )),
        Ok(Err(missing_deps)) => {
            let msg = format!(
                "Missing dependencies: {}",
                missing_deps
                    .into_iter()
                    .map(|dep| format!("{}", dep))
                    .collect::<std::vec::Vec<_>>()
                    .join(", ")
            );
            Err(ExecutionError::new_with_source(
                ExecutionErrorKind::PublishUpgradeMissingDependency,
                msg,
            ))
        }
        Ok(Ok(pkgs)) => Ok(pkgs),
    }
}

/// `sui_types::storage::get_package_objects`: the packages, or the ids that are not packages in
/// the store.
#[allow(clippy::type_complexity)]
fn get_package_objects<'ctx, 'a>(
    bump: &'a Bump,
    store: &dyn BackingPackageStore<'a>,
    package_ids: impl IntoIterator<Item = &'ctx ObjectId>,
) -> exec_types::storage::SuiResult<Result<Vec<'a, Object<'a>>, Vec<'a, ObjectId>>> {
    let mut fetched = Vec::new_in(bump);
    let mut failed_to_fetch = Vec::new_in(bump);
    for id in package_ids {
        match store.get_package_object(id)? {
            None => failed_to_fetch.push(*id),
            Some(o) => fetched.push(o),
        }
    }
    if !failed_to_fetch.is_empty() {
        Ok(Err(failed_to_fetch))
    } else {
        Ok(Ok(fetched))
    }
}

pub fn check_compatibility<'a>(
    bump: &'a Bump,
    protocol_config: &ProtocolConfig,
    existing_package: &MovePackage<'_>,
    upgrading_modules: &[CompiledModule],
    policy: u8,
) -> Result<(), ExecutionError<'a>> {
    // Make sure this is a known upgrade policy.
    let Ok(policy) = UpgradePolicy::try_from(policy) else {
        return Err(ExecutionError::from_kind(
            ExecutionErrorKind::PackageUpgradeError {
                upgrade_error: PackageUpgradeError::UnknownUpgradePolicy { policy },
            },
        ));
    };

    let pool = &mut normalized::RcPool::new();
    let binary_config = protocol_config.binary_config(None);
    let Ok(current_normalized) = move_package::normalize(
        bump,
        existing_package,
        pool,
        &binary_config,
        /* include code */ true,
    ) else {
        invariant_violation!("Tried to normalize modules in existing package but failed")
    };

    let existing_modules_len = current_normalized.len();
    let upgrading_modules_len = upgrading_modules.len();
    let disallow_new_modules = policy as u8 == UpgradePolicy::DEP_ONLY;

    if disallow_new_modules && existing_modules_len != upgrading_modules_len {
        return Err(ExecutionError::new_with_source(
            ExecutionErrorKind::PackageUpgradeError {
                upgrade_error: PackageUpgradeError::IncompatibleUpgrade,
            },
            format!(
                "Existing package has {existing_modules_len} modules, but new package has \
                     {upgrading_modules_len}. Adding or removing a module to a deps only package is not allowed."
            ),
        ));
    }

    let mut new_normalized = move_package::normalize_deserialized_modules(
        bump,
        pool,
        upgrading_modules.iter(),
        /* include code */ true,
    );
    for (name, cur_module) in current_normalized {
        let Some(new_module) = new_normalized.remove(&name) else {
            return Err(ExecutionError::new_with_source(
                ExecutionErrorKind::PackageUpgradeError {
                    upgrade_error: PackageUpgradeError::IncompatibleUpgrade,
                },
                format!("Existing module {name} not found in next version of package"),
            ));
        };

        check_module_compatibility(&policy, &cur_module, &new_module)?;
    }

    // If we disallow new modules double check that there are no modules left in `new_normalized`.
    debug_assert!(!disallow_new_modules || new_normalized.is_empty());

    Ok(())
}

fn check_module_compatibility<'a>(
    policy: &UpgradePolicy,
    cur_module: &move_binary_format::compatibility::Module,
    new_module: &move_binary_format::compatibility::Module,
) -> Result<(), ExecutionError<'a>> {
    match policy {
        UpgradePolicy::Additive => InclusionCheck::Subset.check(cur_module, new_module),
        UpgradePolicy::DepOnly => InclusionCheck::Equal.check(cur_module, new_module),
        UpgradePolicy::Compatible => {
            let compatibility = Compatibility::upgrade_check();

            compatibility.check(cur_module, new_module)
        }
    }
    .map_err(|e| {
        ExecutionError::new_with_source(
            ExecutionErrorKind::PackageUpgradeError {
                upgrade_error: PackageUpgradeError::IncompatibleUpgrade,
            },
            e,
        )
    })
}

/// Assert the type inferred matches the object's type. This has already been done during loading,
/// but is checked again as an invariant. This may be removed safely at a later time if needed.
// The reference reads the full type through `MoveObjectType`'s accessors; here the compact
// framework types are expanded in `bump`.
fn assert_expected_move_object_type<'a>(
    bump: &'a Bump,
    actual: &Type<'_>,
    expected: &MoveObjectType<'a>,
) -> Result<(), ExecutionError<'a>> {
    let Type::Datatype(actual) = actual else {
        invariant_violation!("Expected a datatype for a Move object");
    };
    let expected_tag = move_object_type_struct_tag_in(bump, expected);
    let (a, m, n) = actual.qualified_ident();
    assert_invariant!(
        a.into_bytes() == expected_tag.address.0,
        "Actual address does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    assert_invariant!(
        m == expected_tag.module,
        "Actual module does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    assert_invariant!(
        n == expected_tag.name,
        "Actual struct does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    let actual_type_arguments = &actual.type_arguments;
    let expected_type_arguments = expected_tag.type_params;
    assert_invariant!(
        actual_type_arguments.len() == expected_type_arguments.len(),
        "Actual type arg length does not match expected. \
       actual: {actual:?} vs expected: {expected:?}",
    );
    // Lengths checked above.
    #[allow(clippy::disallowed_methods)]
    for (actual_ty, expected_ty) in actual_type_arguments.iter().zip(expected_type_arguments) {
        assert_expected_type(actual_ty, expected_ty)?;
    }
    Ok(())
}

/// Assert the type inferred matches the expected type. This has already been done during typing,
/// but is checked again as an invariant. This may be removed safely at a later time if needed.
fn assert_expected_type<'a>(
    actual: &Type<'_>,
    expected: &TypeTag<'_>,
) -> Result<(), ExecutionError<'a>> {
    match (actual, expected) {
        (Type::Bool, TypeTag::Bool)
        | (Type::U8, TypeTag::U8)
        | (Type::U16, TypeTag::U16)
        | (Type::U32, TypeTag::U32)
        | (Type::U64, TypeTag::U64)
        | (Type::U128, TypeTag::U128)
        | (Type::U256, TypeTag::U256)
        | (Type::Address, TypeTag::Address)
        | (Type::Signer, TypeTag::Signer) => Ok(()),
        (Type::Vector(inner_actual), TypeTag::Vector(inner_expected)) => {
            assert_expected_type(&inner_actual.element_type, inner_expected)
        }
        (Type::Datatype(actual_dt), TypeTag::Struct(expected_st)) => {
            assert_expected_data_type(actual_dt, expected_st)
        }
        _ => invariant_violation!(
            "Type mismatch between actual: {actual:?} and expected: {expected:?}"
        ),
    }
}
/// Assert the type inferred matches the expected type. This has already been done during typing,
/// but is checked again as an invariant. This may be removed safely at a later time if needed.
fn assert_expected_data_type<'a>(
    actual: &Datatype<'_>,
    expected: &StructTag<'_>,
) -> Result<(), ExecutionError<'a>> {
    let (a, m, n) = actual.qualified_ident();
    assert_invariant!(
        a.into_bytes() == expected.address.0,
        "Actual address does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    assert_invariant!(
        m == expected.module,
        "Actual module does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    assert_invariant!(
        n == expected.name,
        "Actual struct does not match expected. actual: {actual:?} vs expected: {expected:?}"
    );
    let actual_type_arguments = &actual.type_arguments;
    let expected_type_arguments = &expected.type_params;
    assert_invariant!(
        actual_type_arguments.len() == expected_type_arguments.len(),
        "Actual type arg length does not match expected. \
       actual: {actual:?} vs expected: {expected:?}",
    );
    // Lengths checked above.
    #[allow(clippy::disallowed_methods)]
    for (actual_ty, expected_ty) in actual_type_arguments
        .iter()
        .zip(expected_type_arguments.iter())
    {
        assert_expected_type(actual_ty, expected_ty)?;
    }
    Ok(())
}
