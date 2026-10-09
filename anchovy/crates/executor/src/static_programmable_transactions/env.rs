// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This module defines the shared environment, `Env`, used for the compilation/translation and
//! execution of programmable transactions. While the "context" for each pass might be different,
//! the `Env` provides consistent access to shared components such as the VM or the protocol config.

use crate::{
    data_store::{VerifiedPackageStore, cached_package_store::CachedPackageStore},
    execution_mode::ExecutionMode,
    execution_value::ExecutionState,
    static_programmable_transactions::{
        execution::context::subst_signature,
        linkage::{analysis::LinkageAnalyzer, resolved_linkage::ExecutableLinkage},
        loading::ast::{
            self as L, Datatype, DeserializedPackage, LoadedFunction, LoadedFunctionInstantiation,
            ModuleId, Type,
        },
    },
};
use containers::{Bump, Vec};
use exec_types::base::object_id;
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use exec_types::object::{Object, compute_digest_for_modules_and_deps};
use exec_types::{assert_invariant, checked_as, invariant_violation};
use messages::base::ObjectId;
use messages::execution_status::TypeArgumentError;
use messages::type_tag::TypeInput;
use move_binary_format::{
    CompiledModule,
    errors::{Location, VMError, VMResult},
    file_format::{Ability, AbilitySet, TypeParameterIndex},
};
use move_core_types::{
    annotated_value,
    identifier::{IdentStr, Identifier},
    language_storage::{self as move_tags, StructTag, TypeTag},
    resolver::IntraPackageName,
    runtime_value::{self, MoveTypeLayout},
    vm_status::StatusCode,
};
use move_vm_runtime::{
    execution::{self as vm_runtime, vm::MoveVM},
    runtime::MoveRuntime,
};
use std::{
    cell::{OnceCell, RefCell},
    marker::PhantomData,
};
use sui_protocol_config::ProtocolConfig;
use sui_types::{
    allowance::RESOLVED_ALLOWANCE_WITHDRAWAL_STRUCT,
    balance::RESOLVED_BALANCE_STRUCT,
    base_types::TxContext,
    coin::RESOLVED_COIN_STRUCT,
    funds_accumulator::RESOLVED_WITHDRAWAL_STRUCT,
    gas_coin::GasCoin,
    move_package::{UpgradeCap, UpgradeReceipt, UpgradeTicket},
};

pub struct Env<'a, 'pc, 'vm, 'state, 'linkage, 'extensions, Mode>
where
    Mode: ExecutionMode,
{
    /// The transaction's arena.
    pub bump: &'a Bump,
    pub protocol_config: &'pc ProtocolConfig,
    pub vm: &'vm MoveRuntime,
    pub state_view: &'state mut dyn ExecutionState<'a>,
    pub linkable_store: &'linkage CachedPackageStore<'a, 'vm>,
    pub linkage_analysis: &'linkage LinkageAnalyzer<'a>,
    gas_coin_type: OnceCell<Type<'a>>,
    upgrade_ticket_type: OnceCell<Type<'a>>,
    upgrade_receipt_type: OnceCell<Type<'a>>,
    upgrade_cap_type: OnceCell<Type<'a>>,
    tx_context_type: OnceCell<Type<'a>>,
    /// Type linkages computed while no package was published in the transaction, by the ordered
    /// list of package IDs they were computed for. Not in the reference, which recomputes one for
    /// every layout and type load.
    ///
    /// A hit is the linkage `type_linkage` would compute: it depends only on the IDs, in order,
    /// the resolution config, which is fixed for the transaction, and the packages the store
    /// returns for them. With no published packages the store returns the runtime's immutable
    /// package for an ID (see `CachedPackageStore`), so entries are made and used only while
    /// `new_packages` is empty. Errors are not kept.
    type_linkages: RefCell<Vec<'a, (&'a [ObjectId], ExecutableLinkage<'a>)>>,
    // The VM used for type resolution of input types (and types statically present in the PTB)
    // only. This VM should only be used for resolution of input types, but should not be used for
    // resolution around function calls, execution, or final serialization of execution values.
    input_type_resolution_vm: &'linkage MoveVM<'extensions>,
    _mode: PhantomData<fn() -> Mode>,
}

macro_rules! get_or_init_ty {
    ($env:expr, $ident:ident, $tag:expr) => {{
        let env = $env;
        if env.$ident.get().is_none() {
            let tag = $tag;
            let ty = env.load_type_from_struct(tag)?;
            env.$ident.set(ty).unwrap();
        }
        Ok(*env.$ident.get().unwrap())
    }};
}

impl<'a, 'pc, 'vm, 'state, 'linkage, 'extensions, Mode>
    Env<'a, 'pc, 'vm, 'state, 'linkage, 'extensions, Mode>
where
    Mode: ExecutionMode,
{
    pub fn new(
        bump: &'a Bump,
        protocol_config: &'pc ProtocolConfig,
        vm: &'vm MoveRuntime,
        state_view: &'state mut dyn ExecutionState<'a>,
        linkable_store: &'linkage CachedPackageStore<'a, 'vm>,
        linkage_analysis: &'linkage LinkageAnalyzer<'a>,
        input_type_resolution_vm: &'linkage MoveVM<'extensions>,
    ) -> Self {
        Self {
            bump,
            protocol_config,
            vm,
            state_view,
            linkable_store,
            linkage_analysis,
            gas_coin_type: OnceCell::new(),
            upgrade_ticket_type: OnceCell::new(),
            upgrade_receipt_type: OnceCell::new(),
            upgrade_cap_type: OnceCell::new(),
            tx_context_type: OnceCell::new(),
            type_linkages: RefCell::new(Vec::new_in(bump)),
            input_type_resolution_vm,
            _mode: PhantomData,
        }
    }

    pub fn convert_linked_vm_error(
        &self,
        e: VMError,
        linkage: &ExecutableLinkage<'_>,
    ) -> ExecutionError<'a> {
        convert_vm_error(
            self.bump,
            e,
            self.linkable_store,
            Some(linkage),
            self.protocol_config,
        )
    }

    pub fn convert_vm_error(&self, e: VMError) -> ExecutionError<'a> {
        convert_vm_error(
            self.bump,
            e,
            self.linkable_store,
            None,
            self.protocol_config,
        )
    }

    pub fn convert_type_argument_error(
        &self,
        idx: usize,
        e: VMError,
        linkage: &ExecutableLinkage<'_>,
    ) -> ExecutionError<'a> {
        let argument_idx = match checked_as!(idx, TypeParameterIndex) {
            Err(e) => return e,
            Ok(v) => v,
        };
        match e.major_status() {
            StatusCode::NUMBER_OF_TYPE_ARGUMENTS_MISMATCH => {
                ExecutionError::from_kind(ExecutionErrorKind::TypeArityMismatch)
            }
            StatusCode::EXTERNAL_RESOLUTION_REQUEST_ERROR => {
                ExecutionError::from_kind(ExecutionErrorKind::TypeArgumentError {
                    argument_idx,
                    kind: TypeArgumentError::TypeNotFound,
                })
            }
            StatusCode::CONSTRAINT_NOT_SATISFIED => {
                ExecutionError::from_kind(ExecutionErrorKind::TypeArgumentError {
                    argument_idx,
                    kind: TypeArgumentError::ConstraintNotSatisfied,
                })
            }
            _ => self.convert_linked_vm_error(e, linkage),
        }
    }

    pub fn fully_annotated_layout(
        &self,
        ty: &Type<'a>,
    ) -> Result<annotated_value::MoveTypeLayout, ExecutionError<'a>> {
        let tag: TypeTag = (*ty).try_into().map_err(|s| {
            ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, s)
        })?;
        let objects = tag_addresses(self.bump, &tag);
        let tag_linkage = self.type_linkage(&objects)?;
        self.input_type_resolution_vm
            .annotated_type_layout(&tag)
            .map_err(|e| self.convert_linked_vm_error(e, &tag_linkage))
    }

    pub fn runtime_layout(
        &self,
        ty: &Type<'a>,
    ) -> Result<runtime_value::MoveTypeLayout, ExecutionError<'a>> {
        let tag: TypeTag = (*ty).try_into().map_err(|s| {
            ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, s)
        })?;
        let objects = tag_addresses(self.bump, &tag);
        let tag_linkage = self.type_linkage(&objects)?;
        self.input_type_resolution_vm
            .runtime_type_layout(&tag)
            .map_err(|e| self.convert_linked_vm_error(e, &tag_linkage))
    }

    /// `ExecutableLinkage::type_linkage` for `ids`, memoized (see `type_linkages`).
    fn type_linkage(&self, ids: &[ObjectId]) -> Result<ExecutableLinkage<'a>, ExecutionError<'a>> {
        let compute = || {
            ExecutableLinkage::type_linkage(
                *self.linkage_analysis.config(),
                ids,
                self.linkable_store,
            )
        };
        let memoize = !self.linkable_store.package_store.has_new_packages();
        if memoize
            && let Some((_, linkage)) = self
                .type_linkages
                .borrow()
                .iter()
                .find(|(key, _)| *key == ids)
        {
            debug_assert!(compute().is_ok_and(|fresh| {
                fresh.0.linkage.iter().eq(linkage.0.linkage.iter())
                    && fresh
                        .0
                        .linkage_resolution
                        .iter()
                        .eq(linkage.0.linkage_resolution.iter())
            }));
            return Ok(*linkage);
        }
        let linkage = compute()?;
        if memoize {
            self.type_linkages
                .borrow_mut()
                .push((containers::alloc_slice_copy(self.bump, ids), linkage));
        }
        Ok(linkage)
    }

    pub fn load_framework_function(
        &self,
        module: &'a IdentStr,
        function: &'a IdentStr,
        type_arguments: Vec<'a, Type<'a>>,
        unified_linkage: Option<&ExecutableLinkage<'a>>,
    ) -> Result<LoadedFunction<'a>, ExecutionError<'a>> {
        let mut loaded = self.load_function(
            ObjectId::from_u16(0x2),
            module.as_str(),
            function.as_str(),
            type_arguments,
        )?;
        if self.protocol_config.harden_linkage_consistency() {
            let Some(unified_linkage) = unified_linkage else {
                invariant_violation!(
                    "Unified linkage is required when hardened linkage consistency is enabled"
                )
            };
            assert_invariant!(
                loaded
                    .linkage
                    .0
                    .linkage
                    .keys()
                    .all(|original_id| unified_linkage.0.linkage.contains_key(original_id)),
                "transaction linkage drops a package resolved by a framework MoveCall"
            );
            loaded.linkage = *unified_linkage;
        }
        Ok(loaded)
    }

    pub fn load_function(
        &self,
        package: ObjectId,
        module: &'a str,
        function: &'a str,
        type_arguments: Vec<'a, Type<'a>>,
    ) -> Result<LoadedFunction<'a>, ExecutionError<'a>> {
        let module_ident = to_identifier(module)?;
        let name = to_identifier(function)?;

        let linkage = self.linkage_analysis.compute_call_linkage(
            &package,
            module_ident.as_ident_str(),
            name.as_ident_str(),
            &type_arguments,
            self.linkable_store,
        )?;

        let Some(original_id) = linkage.0.resolve_to_original_id(&package) else {
            invariant_violation!(
                "Package ID {:?} is not found in linkage generated for that package",
                package
            );
        };
        let version_mid = ModuleId {
            address: exec_types::base::move_address(&package),
            name: module,
        };
        let original_mid = ModuleId {
            address: exec_types::base::move_address(&original_id),
            name: module,
        };
        let loaded_type_arguments = type_arguments
            .iter()
            .enumerate()
            .map(|(idx, ty)| self.load_vm_type_argument_from_adapter_type(idx, ty))
            .collect::<Result<std::vec::Vec<_>, _>>()?;
        // NB: We cannot use the resolution VM here because the linkage for that unifies up, and if
        // this is a private entry function, it may have been removed in future versions of the
        // package.
        let vm = self
            .vm
            .make_vm(
                &self.linkable_store.package_store,
                linkage.linkage_context()?,
            )
            .map_err(|e| self.convert_linked_vm_error(e, &linkage))?;
        let original_vm_mid = move_tags::ModuleId::new(original_mid.address, module_ident);
        let runtime_signature = vm
            .function_information(
                &original_vm_mid,
                name.as_ident_str(),
                &loaded_type_arguments,
            )
            .map_err(|e| {
                if e.major_status() == StatusCode::EXTERNAL_RESOLUTION_REQUEST_ERROR {
                    ExecutionError::new_with_source(
                        ExecutionErrorKind::FunctionNotFound,
                        format!(
                            "Could not resolve function '{}' in module '{}::{}'",
                            name,
                            version_mid.address.to_canonical_display(true),
                            version_mid.name,
                        ),
                    )
                } else {
                    self.convert_linked_vm_error(e, &linkage)
                }
            })?;
        let runtime_signature = subst_signature(runtime_signature, &loaded_type_arguments)
            .map_err(|e| self.convert_linked_vm_error(e, &linkage))?;
        let mut parameters = Vec::with_capacity_in(runtime_signature.parameters.len(), self.bump);
        for ty in &runtime_signature.parameters {
            parameters.push(self.adapter_type_from_vm_type(&vm, ty)?);
        }
        let mut return_ = Vec::with_capacity_in(runtime_signature.return_.len(), self.bump);
        for ty in &runtime_signature.return_ {
            return_.push(self.adapter_type_from_vm_type(&vm, ty)?);
        }
        let signature = LoadedFunctionInstantiation {
            parameters,
            return_,
        };
        Ok(LoadedFunction {
            version_mid,
            original_mid,
            name: function,
            type_arguments,
            signature,
            linkage,
            instruction_length: runtime_signature.instruction_count,
            definition_index: runtime_signature.index,
            visibility: runtime_signature.visibility,
            is_entry: runtime_signature.is_entry,
            is_native: runtime_signature.is_native,
        })
    }

    pub fn load_type_input(
        &self,
        idx: usize,
        ty: TypeInput<'_>,
    ) -> Result<Type<'a>, ExecutionError<'a>> {
        let vm_type = self.load_vm_type_from_type_input(idx, ty)?;
        self.adapter_type_from_vm_type(self.input_type_resolution_vm, &vm_type)
    }

    pub fn load_type_tag(&self, idx: usize, ty: &TypeTag) -> Result<Type<'a>, ExecutionError<'a>> {
        let vm_type = self.load_vm_type_from_type_tag(Some(idx), ty)?;
        self.adapter_type_from_vm_type(self.input_type_resolution_vm, &vm_type)
    }

    /// We verify that all types in the `StructTag` are defining ID-based types.
    ///
    /// The reference borrows the tag and clones it; every caller has an owned tag to give.
    pub fn load_type_from_struct(&self, tag: StructTag) -> Result<Type<'a>, ExecutionError<'a>> {
        let vm_type = self.load_vm_type_from_type_tag(None, &TypeTag::Struct(Box::new(tag)))?;
        self.adapter_type_from_vm_type(self.input_type_resolution_vm, &vm_type)
    }

    pub fn type_layout_for_struct(
        &self,
        tag: StructTag,
    ) -> Result<MoveTypeLayout, ExecutionError<'a>> {
        let ty: Type = self.load_type_from_struct(tag)?;
        self.runtime_layout(&ty)
    }

    pub fn gas_coin_type(&self) -> Result<Type<'a>, ExecutionError<'a>> {
        get_or_init_ty!(self, gas_coin_type, GasCoin::type_())
    }

    pub fn upgrade_ticket_type(&self) -> Result<Type<'a>, ExecutionError<'a>> {
        get_or_init_ty!(self, upgrade_ticket_type, UpgradeTicket::type_())
    }

    pub fn upgrade_receipt_type(&self) -> Result<Type<'a>, ExecutionError<'a>> {
        get_or_init_ty!(self, upgrade_receipt_type, UpgradeReceipt::type_())
    }

    pub fn upgrade_cap_type(&self) -> Result<Type<'a>, ExecutionError<'a>> {
        get_or_init_ty!(self, upgrade_cap_type, UpgradeCap::type_())
    }

    pub fn tx_context_type(&self) -> Result<Type<'a>, ExecutionError<'a>> {
        get_or_init_ty!(self, tx_context_type, TxContext::type_())
    }

    /// One of the framework's datatypes with one type argument, from its `RESOLVED_*` name: the
    /// name is static, so nothing but the node and its argument is allocated.
    fn framework_datatype(
        &self,
        abilities: AbilitySet,
        (a, m, n): (
            &'static move_core_types::account_address::AccountAddress,
            &'static IdentStr,
            &'static IdentStr,
        ),
        inner_type: Type<'a>,
    ) -> Type<'a> {
        Type::Datatype(containers::leak(
            self.bump,
            Datatype {
                abilities,
                module: ModuleId {
                    address: *a,
                    name: m.as_str(),
                },
                name: n.as_str(),
                type_arguments: containers::alloc_slice_copy(self.bump, &[inner_type]),
            },
        ))
    }

    pub fn coin_type(&self, inner_type: Type<'a>) -> Result<Type<'a>, ExecutionError<'a>> {
        const COIN_ABILITIES: AbilitySet =
            AbilitySet::singleton(Ability::Key).union(AbilitySet::singleton(Ability::Store));
        Ok(self.framework_datatype(COIN_ABILITIES, RESOLVED_COIN_STRUCT, inner_type))
    }

    pub fn balance_type(&self, inner_type: Type<'a>) -> Result<Type<'a>, ExecutionError<'a>> {
        const BALANCE_ABILITIES: AbilitySet = AbilitySet::singleton(Ability::Store);
        Ok(self.framework_datatype(BALANCE_ABILITIES, RESOLVED_BALANCE_STRUCT, inner_type))
    }

    pub fn withdrawal_type(&self, inner_type: Type<'a>) -> Result<Type<'a>, ExecutionError<'a>> {
        const WITHDRAWAL_ABILITIES: AbilitySet = AbilitySet::singleton(Ability::Drop);
        Ok(self.framework_datatype(WITHDRAWAL_ABILITIES, RESOLVED_WITHDRAWAL_STRUCT, inner_type))
    }

    pub fn allowance_withdrawal_type(
        &self,
        inner_type: Type<'a>,
    ) -> Result<Type<'a>, ExecutionError<'a>> {
        const ALLOWANCE_WITHDRAWAL_ABILITIES: AbilitySet = AbilitySet::singleton(Ability::Drop);
        Ok(self.framework_datatype(
            ALLOWANCE_WITHDRAWAL_ABILITIES,
            RESOLVED_ALLOWANCE_WITHDRAWAL_STRUCT,
            inner_type,
        ))
    }

    /// Either `Withdrawal` or `AllowanceWithdrawal` depending on the source
    pub fn withdrawal_type_for_source(
        &self,
        source: &L::WithdrawalSource,
        funds_type: Type<'a>,
    ) -> Result<Type<'a>, ExecutionError<'a>> {
        match source {
            L::WithdrawalSource::Direct { .. } => self.withdrawal_type(funds_type),
            L::WithdrawalSource::Allowance { .. } => self.allowance_withdrawal_type(funds_type),
        }
    }

    pub fn vector_type(&self, element_type: Type<'a>) -> Result<Type<'a>, ExecutionError<'a>> {
        let abilities = AbilitySet::polymorphic_abilities(
            AbilitySet::VECTOR,
            [false],
            [element_type.abilities()],
        )
        .map_err(|e| {
            ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, e.to_string())
        })?;
        Ok(Type::Vector(containers::leak(
            self.bump,
            L::Vector {
                abilities,
                element_type,
            },
        )))
    }

    pub fn read_object(&self, id: &ObjectId) -> Result<&Object<'a>, ExecutionError<'a>> {
        let Some(obj) = self.state_view.read_object(id) else {
            // protected by transaction input checker
            invariant_violation!("Object {:?} does not exist", id);
        };
        Ok(obj)
    }

    /// Takes an adapter Type and returns a VM runtime Type and the linkage for it.
    pub fn load_vm_type_argument_from_adapter_type(
        &self,
        idx: usize,
        ty: &Type<'a>,
    ) -> Result<vm_runtime::Type, ExecutionError<'a>> {
        self.load_vm_type_from_adapter_type(Some(idx), ty)
    }

    fn load_vm_type_from_adapter_type(
        &self,
        type_arg_idx: Option<usize>,
        ty: &Type<'a>,
    ) -> Result<vm_runtime::Type, ExecutionError<'a>> {
        let tag: TypeTag = (*ty).try_into().map_err(|s| {
            ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, s)
        })?;
        self.load_vm_type_from_type_tag(type_arg_idx, &tag)
    }

    /// Take a type tag and returns a VM runtime Type and the linkage for it.
    fn load_vm_type_from_type_tag(
        &self,
        type_arg_idx: Option<usize>,
        tag: &TypeTag,
    ) -> Result<vm_runtime::Type, ExecutionError<'a>> {
        fn execution_error<'a, Mode: ExecutionMode>(
            env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
            type_arg_idx: Option<usize>,
            e: VMError,
            linkage: &ExecutableLinkage<'_>,
        ) -> ExecutionError<'a> {
            if let Some(idx) = type_arg_idx {
                env.convert_type_argument_error(idx, e, linkage)
            } else {
                env.convert_linked_vm_error(e, linkage)
            }
        }

        let objects = tag_addresses(self.bump, tag);

        let tag_linkage = self.type_linkage(&objects)?;
        let ty = self
            .input_type_resolution_vm
            .load_type(tag)
            .map_err(|e| execution_error(self, type_arg_idx, e, &tag_linkage))?;
        Ok(ty)
    }

    /// Converts a VM runtime Type to an adapter Type.
    pub(crate) fn adapter_type_from_vm_type(
        &self,
        vm: &MoveVM,
        vm_type: &vm_runtime::Type,
    ) -> Result<Type<'a>, ExecutionError<'a>> {
        use vm_runtime as VRT;

        Ok(match vm_type {
            VRT::Type::Bool => Type::Bool,
            VRT::Type::U8 => Type::U8,
            VRT::Type::U16 => Type::U16,
            VRT::Type::U32 => Type::U32,
            VRT::Type::U64 => Type::U64,
            VRT::Type::U128 => Type::U128,
            VRT::Type::U256 => Type::U256,
            VRT::Type::Address => Type::Address,
            VRT::Type::Signer => Type::Signer,

            VRT::Type::Reference(ref_ty) => {
                let inner_ty = self.adapter_type_from_vm_type(vm, ref_ty)?;
                Type::Reference(false, containers::alloc(self.bump, inner_ty))
            }
            VRT::Type::MutableReference(ref_ty) => {
                let inner_ty = self.adapter_type_from_vm_type(vm, ref_ty)?;
                Type::Reference(true, containers::alloc(self.bump, inner_ty))
            }

            VRT::Type::Vector(inner) => {
                let element_type = self.adapter_type_from_vm_type(vm, inner)?;
                self.vector_type(element_type)?
            }
            VRT::Type::Datatype(_) => {
                let type_information = vm
                    .type_information(vm_type)
                    .map_err(|e| self.convert_vm_error(e))?;
                let Some(data_type_info) = type_information.datatype_info else {
                    invariant_violation!("Expected datatype info for datatype type {:?}", vm_type);
                };
                let datatype = Datatype {
                    abilities: type_information.abilities,
                    module: ModuleId {
                        address: data_type_info.defining_id,
                        name: containers::alloc_str(self.bump, data_type_info.module_name.as_str()),
                    },
                    name: containers::alloc_str(self.bump, data_type_info.type_name.as_str()),
                    type_arguments: &[],
                };
                Type::Datatype(containers::leak(self.bump, datatype))
            }
            ty @ VRT::Type::DatatypeInstantiation(inst) => {
                let (_, type_arguments) = &**inst;
                let type_information = vm
                    .type_information(ty)
                    .map_err(|e| self.convert_vm_error(e))?;
                let Some(data_type_info) = type_information.datatype_info else {
                    invariant_violation!("Expected datatype info for datatype type {:?}", vm_type);
                };

                let abilities = type_information.abilities;
                let module = ModuleId {
                    address: data_type_info.defining_id,
                    name: containers::alloc_str(self.bump, data_type_info.module_name.as_str()),
                };
                let name = containers::alloc_str(self.bump, data_type_info.type_name.as_str());
                let mut adapter_type_arguments =
                    Vec::with_capacity_in(type_arguments.len(), self.bump);
                for t in type_arguments.iter() {
                    adapter_type_arguments.push(self.adapter_type_from_vm_type(vm, t)?);
                }

                Type::Datatype(containers::leak(
                    self.bump,
                    Datatype {
                        abilities,
                        module,
                        name,
                        type_arguments: adapter_type_arguments.leak(),
                    },
                ))
            }

            VRT::Type::TyParam(_) => {
                invariant_violation!(
                    "Unexpected type parameter in VM type: {:?}. This should not happen as we should \
                     have resolved all type parameters before this point.",
                    vm_type
                );
            }
        })
    }

    /// Load a `TypeInput` into a VM runtime `Type` and its `Linkage`. Loading into the VM ensures
    /// that any adapter type or type tag that results from this is properly output with defining
    /// IDs.
    fn load_vm_type_from_type_input(
        &self,
        type_arg_idx: usize,
        ty: TypeInput<'_>,
    ) -> Result<vm_runtime::Type, ExecutionError<'a>> {
        fn to_type_tag_internal<'a, Mode: ExecutionMode>(
            env: &Env<'a, '_, '_, '_, '_, '_, Mode>,
            type_arg_idx: usize,
            ty: TypeInput<'_>,
        ) -> Result<TypeTag, ExecutionError<'a>> {
            Ok(match ty {
                TypeInput::Bool => TypeTag::Bool,
                TypeInput::U8 => TypeTag::U8,
                TypeInput::U16 => TypeTag::U16,
                TypeInput::U32 => TypeTag::U32,
                TypeInput::U64 => TypeTag::U64,
                TypeInput::U128 => TypeTag::U128,
                TypeInput::U256 => TypeTag::U256,
                TypeInput::Address => TypeTag::Address,
                TypeInput::Signer => TypeTag::Signer,
                TypeInput::Vector(type_input) => {
                    let inner = to_type_tag_internal(env, type_arg_idx, *type_input)?;
                    TypeTag::Vector(Box::new(inner))
                }
                TypeInput::Struct(struct_input) => {
                    let messages::type_tag::StructInput {
                        address,
                        module,
                        name,
                        type_params,
                    } = *struct_input;

                    let pkg = env
                        .linkable_store
                        .get_package(&ObjectId(address.0))
                        .ok()
                        .flatten()
                        .ok_or_else(|| {
                            let argument_idx = match checked_as!(type_arg_idx, u16) {
                                Err(e) => return e,
                                Ok(v) => v,
                            };
                            ExecutionError::from_kind(ExecutionErrorKind::TypeArgumentError {
                                argument_idx,
                                kind: TypeArgumentError::TypeNotFound,
                            })
                        })?;
                    let module = to_identifier(module)?;
                    let name = to_identifier(name)?;
                    let tid = IntraPackageName {
                        module_name: module,
                        type_name: name,
                    };
                    let Some(resolved_address) = pkg.type_origin_table().get(&tid).cloned() else {
                        return Err(ExecutionError::from_kind(
                            ExecutionErrorKind::TypeArgumentError {
                                argument_idx: checked_as!(type_arg_idx, u16)?,
                                kind: TypeArgumentError::TypeNotFound,
                            },
                        ));
                    };

                    let tys = type_params
                        .iter()
                        .map(|tp| to_type_tag_internal(env, type_arg_idx, *tp))
                        .collect::<Result<std::vec::Vec<_>, _>>()?;
                    TypeTag::Struct(Box::new(StructTag {
                        address: resolved_address,
                        module: tid.module_name,
                        name: tid.type_name,
                        type_params: tys,
                    }))
                }
            })
        }
        let tag = to_type_tag_internal(self, type_arg_idx, ty)?;
        self.load_vm_type_from_type_tag(Some(type_arg_idx), &tag)
    }

    pub fn deserialize_package(
        &self,
        module_bytes: &[&[u8]],
        dep_ids: &[ObjectId],
    ) -> Result<DeserializedPackage<'a>, ExecutionError<'a>> {
        assert_invariant!(
            !module_bytes.is_empty(),
            "empty package is checked in transaction input checker"
        );

        let total_bytes = module_bytes.iter().map(|v| v.len()).sum();

        let binary_config = self.protocol_config.binary_config(None);
        let mut deserialized_modules = Vec::with_capacity_in(module_bytes.len(), self.bump);
        for b in module_bytes {
            let module: VMResult<CompiledModule> =
                CompiledModule::deserialize_with_config(b, &binary_config)
                    .map_err(|e| e.finish(Location::Undefined));
            deserialized_modules.push(module.map_err(|e| self.convert_vm_error(e))?);
        }
        let computed_digest = compute_digest_for_modules_and_deps(
            self.bump,
            module_bytes,
            dep_ids,
            /* hash_modules */ true,
        );
        Ok(DeserializedPackage::new(
            self.bump,
            deserialized_modules,
            total_bytes,
            computed_digest,
        ))
    }
}

fn to_identifier(name: &str) -> Result<Identifier, ExecutionError<'static>> {
    Identifier::new(name).map_err(|e| {
        ExecutionError::new_with_source(ExecutionErrorKind::VMInvariantViolation, e.to_string())
    })
}

/// `TypeTag::all_addresses` as package IDs in the arena: each address once, in pre-order of
/// first occurrence.
fn tag_addresses<'a>(bump: &'a Bump, tag: &TypeTag) -> Vec<'a, ObjectId> {
    fn add(tag: &TypeTag, ids: &mut Vec<'_, ObjectId>) {
        match tag {
            TypeTag::Bool
            | TypeTag::U8
            | TypeTag::U16
            | TypeTag::U32
            | TypeTag::U64
            | TypeTag::U128
            | TypeTag::U256
            | TypeTag::Address
            | TypeTag::Signer => (),
            TypeTag::Vector(inner) => add(inner, ids),
            TypeTag::Struct(s) => {
                let id = object_id(&s.address);
                if !ids.contains(&id) {
                    ids.push(id);
                }
                for param in &s.type_params {
                    add(param, ids);
                }
            }
        }
    }
    let mut ids = Vec::new_in(bump);
    add(tag, &mut ids);
    debug_assert!(
        ids.iter()
            .copied()
            .eq(tag.all_addresses().iter().map(object_id))
    );
    ids
}

fn convert_vm_error<'a>(
    bump: &'a Bump,
    error: VMError,
    store: &VerifiedPackageStore<'_>,
    linkage: Option<&ExecutableLinkage<'_>>,
    _protocol_config: &ProtocolConfig,
) -> ExecutionError<'a> {
    use crate::error::convert_vm_error_impl;
    convert_vm_error_impl(
        bump,
        error,
        &|id| {
            debug_assert!(
                linkage.is_some(),
                "Linkage should be set anywhere where runtime errors may occur in order to resolve abort locations to package IDs"
            );
            linkage
                .and_then(|linkage| {
                    linkage
                        .0
                        .linkage
                        .get(&object_id(id.address()))
                        .map(|new_id| {
                            move_tags::ModuleId::new(
                                exec_types::base::move_address(new_id),
                                id.name().to_owned(),
                            )
                        })
                })
                .unwrap_or_else(|| id.clone())
        },
        // NB: the `id` here is the original ID (and hence _not_ relocated).
        &|id, function| {
            debug_assert!(
                linkage.is_some(),
                "Linkage should be set anywhere where runtime errors may occur in order to resolve abort locations to package IDs"
            );
            linkage.and_then(|linkage| {
                let version_id = linkage
                    .0
                    .linkage
                    .get(&object_id(id.address()))
                    .copied()
                    .unwrap_or_else(|| object_id(id.address()));
                store.get_package(&version_id).ok().flatten().and_then(|p| {
                    p.modules().get(id).map(|module| {
                        let module = module.compiled_module();
                        let fdef = module.function_def_at(function);
                        let fhandle = module.function_handle_at(fdef.function);
                        module.identifier_at(fhandle.name).to_string()
                    })
                })
            })
        },
    )
}
