// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    data_store::VerifiedPackageStore,
    execution_mode::ExecutionMode,
    execution_value::ExecutionState,
    static_programmable_transactions::{
        linkage::{
            config::{LinkageConfig, ResolutionConfig},
            resolution::{ResolutionTable, VersionConstraint, add_and_unify, get_package},
            resolved_linkage::{ExecutableLinkage, ResolvedLinkage},
        },
        loading::ast::Type,
    },
};
use containers::Bump;
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use messages::base::ObjectId;
use messages::transaction::ProgrammableTransaction;
use move_binary_format::file_format::Visibility;
use move_core_types::identifier::IdentStr;
use move_vm_runtime::validation::verification::ast::Package as VerifiedPackage;
use std::sync::Arc;
use sui_protocol_config::ProtocolConfig;

#[derive(Debug)]
pub struct LinkageAnalyzer<'a> {
    internal: ResolutionConfig<'a>,
}

impl<'a> LinkageAnalyzer<'a> {
    pub fn new<Mode: ExecutionMode>(
        bump: &'a Bump,
        protocol_config: &ProtocolConfig,
    ) -> Result<Self, ExecutionError<'a>> {
        let always_include_system_packages = !Mode::packages_are_predefined();
        let linkage_config = LinkageConfig::new(
            protocol_config
                .include_special_package_amendments_as_option()
                .clone(),
            always_include_system_packages,
        );
        let binary_config = protocol_config.binary_config(None);
        Ok(Self {
            internal: ResolutionConfig::new(bump, linkage_config, binary_config),
        })
    }

    pub fn compute_call_linkage(
        &self,
        package: &ObjectId,
        module_name: &IdentStr,
        function_name: &IdentStr,
        type_args: &[Type<'a>],
        store: &VerifiedPackageStore<'_>,
    ) -> Result<ExecutableLinkage<'a>, ExecutionError<'a>> {
        Ok(ExecutableLinkage::new(
            self.internal.bump(),
            ResolvedLinkage::from_resolution_table(self.compute_call_linkage_(
                package,
                module_name,
                function_name,
                type_args,
                store,
            )?),
        ))
    }

    pub fn compute_publication_linkage(
        &self,
        deps: &[ObjectId],
        store: &VerifiedPackageStore<'_>,
    ) -> Result<ResolvedLinkage<'a>, ExecutionError<'a>> {
        Ok(ResolvedLinkage::from_resolution_table(
            self.compute_publication_linkage_(deps, store)?,
        ))
    }

    pub fn config(&self) -> &ResolutionConfig<'a> {
        &self.internal
    }

    pub fn compute_input_type_resolution_linkage(
        &self,
        tx: &ProgrammableTransaction<'_>,
        package_store: &VerifiedPackageStore<'_>,
        object_store: &dyn ExecutionState<'a>,
    ) -> Result<ExecutableLinkage<'a>, ExecutionError<'a>> {
        input_type_resolution_analysis::compute_resolution_linkage(
            self,
            tx,
            package_store,
            object_store,
        )
    }

    fn compute_call_linkage_(
        &self,
        package: &ObjectId,
        module_name: &IdentStr,
        function_name: &IdentStr,
        type_args: &[Type<'a>],
        store: &VerifiedPackageStore<'_>,
    ) -> Result<ResolutionTable<'a>, ExecutionError<'a>> {
        let mut resolution_table = self.internal.resolution_table_with_native_packages(store)?;

        fn add_package(
            object_id: &ObjectId,
            store: &VerifiedPackageStore<'_>,
            resolution_table: &mut ResolutionTable<'_>,
            self_resolution_fn: fn(&Arc<VerifiedPackage>) -> Option<VersionConstraint>,
            dep_resolution_fn: fn(&Arc<VerifiedPackage>) -> Option<VersionConstraint>,
        ) -> Result<(), ExecutionError<'static>> {
            let pkg = get_package(object_id, store)?;
            for object_id in resolution_table.config.linkage_table(&pkg).values() {
                let object_id = ObjectId(object_id.into_bytes());
                add_and_unify(&object_id, store, resolution_table, dep_resolution_fn)?;
            }
            add_and_unify(object_id, store, resolution_table, self_resolution_fn)?;
            Ok(())
        }

        let pkg = get_package(package, store)?;
        let fn_not_found_err = || -> ExecutionError<'a> {
            ExecutionError::new_with_source(
                ExecutionErrorKind::FunctionNotFound,
                format!(
                    "Could not resolve function '{}' in module '{}::{}'",
                    function_name, package, module_name
                ),
            )
        };
        let fdef = pkg
            .modules()
            .iter()
            .find(|m| m.0.name() == module_name)
            .ok_or_else(fn_not_found_err)?
            .1
            .compiled_module()
            .find_function_def_by_name(function_name.as_str())
            .ok_or_else(fn_not_found_err)?;

        let dep_resolution_fn = match fdef.1.visibility {
            Visibility::Public => VersionConstraint::at_least,
            Visibility::Private | Visibility::Friend => VersionConstraint::exact,
        };

        add_package(
            package,
            store,
            &mut resolution_table,
            VersionConstraint::exact,
            dep_resolution_fn,
        )?;

        let bump = self.internal.bump();
        for type_defining_id in type_args.iter().flat_map(|ty| ty.all_addresses(bump)) {
            // Type arguments are "at least" constraints
            add_package(
                &exec_types::base::object_id(&type_defining_id),
                store,
                &mut resolution_table,
                VersionConstraint::at_least,
                VersionConstraint::at_least,
            )?;
        }

        Ok(resolution_table)
    }

    /// Compute the linkage for a publish or upgrade command. This is a special case because
    fn compute_publication_linkage_(
        &self,
        deps: &[ObjectId],
        store: &VerifiedPackageStore<'_>,
    ) -> Result<ResolutionTable<'a>, ExecutionError<'a>> {
        let mut resolution_table = self.internal.resolution_table_with_native_packages(store)?;
        for id in deps {
            add_and_unify(id, store, &mut resolution_table, VersionConstraint::exact)?;
        }
        Ok(resolution_table)
    }
}

mod input_type_resolution_analysis {
    use crate::{
        data_store::VerifiedPackageStore,
        execution_value::ExecutionState,
        static_programmable_transactions::linkage::{
            analysis::LinkageAnalyzer,
            resolution::ResolutionTable,
            resolved_linkage::{ExecutableLinkage, ResolvedLinkage},
        },
    };
    use exec_types::base::object_id;
    use exec_types::error::{ExecutionError, ExecutionErrorKind};
    use exec_types::invariant_violation;
    use exec_types::type_tags::{all_addresses, check_type_input, move_object_type_all_addresses};
    use messages::transaction::{
        CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
        WithdrawalTypeArg,
    };
    use messages::type_tag::TypeInput;

    pub(super) fn compute_resolution_linkage<'a>(
        analyzer: &LinkageAnalyzer<'a>,
        tx: &ProgrammableTransaction<'_>,
        package_store: &VerifiedPackageStore<'_>,
        object_store: &dyn ExecutionState<'a>,
    ) -> Result<ExecutableLinkage<'a>, ExecutionError<'a>> {
        let ProgrammableTransaction { inputs, commands } = tx;

        let mut resolution_table = analyzer
            .internal
            .resolution_table_with_native_packages(package_store)?;
        for arg in inputs.iter() {
            input(&mut resolution_table, arg, package_store, object_store)?;
        }

        for cmd in commands.iter() {
            command(&mut resolution_table, cmd, package_store)?;
        }

        Ok(ExecutableLinkage::new(
            analyzer.internal.bump(),
            ResolvedLinkage::from_resolution_table(resolution_table),
        ))
    }

    fn input<'a>(
        resolution_table: &mut ResolutionTable<'a>,
        arg: &CallArg<'_>,
        package_store: &VerifiedPackageStore<'_>,
        object_store: &dyn ExecutionState<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        let bump = resolution_table.config.bump();
        match arg {
            CallArg::Pure(_) | CallArg::Object(ObjectArg::Receiving(_)) => (),
            CallArg::Object(ObjectArg::ImmOrOwnedObject(oref)) => {
                add_object_type(resolution_table, &oref.id, package_store, object_store)?;
            }
            CallArg::Object(ObjectArg::SharedObject(shared)) => {
                add_object_type(resolution_table, &shared.id, package_store, object_store)?;
            }
            CallArg::FundsWithdrawal(f) => match f.type_arg {
                WithdrawalTypeArg::Balance(tag) => {
                    let ids = all_addresses(bump, &tag).into_iter().map(|a| object_id(&a));
                    resolution_table.add_type_linkages_to_table(ids, package_store)?;
                }
            },
        }

        Ok(())
    }

    /// The reference's `ImmOrOwnedObject` and `SharedObject` arm.
    fn add_object_type<'a>(
        resolution_table: &mut ResolutionTable<'a>,
        id: &messages::base::ObjectId,
        package_store: &VerifiedPackageStore<'_>,
        object_store: &dyn ExecutionState<'a>,
    ) -> Result<(), ExecutionError<'a>> {
        let Some(obj) = object_store.read_object(id) else {
            invariant_violation!("Object {:?} not found in object store", id);
        };
        let Some(ty) = obj.type_() else {
            invariant_violation!("Object {:?} has does not have a Move type", id);
        };

        // invariant: the addresses in the type are defining addresses for the types since
        // these are the types of the objects as stored on-chain.
        let ids = move_object_type_all_addresses(resolution_table.config.bump(), ty)
            .into_iter()
            .map(|a| object_id(&a));
        resolution_table.add_type_linkages_to_table(ids, package_store)?;
        Ok(())
    }

    fn command<'a>(
        resolution_table: &mut ResolutionTable<'a>,
        command: &Command<'_>,
        package_store: &VerifiedPackageStore<'_>,
    ) -> Result<(), ExecutionError<'a>> {
        let bump = resolution_table.config.bump();
        let mut add_ty_input = |ty: &TypeInput<'_>| -> Result<(), ExecutionError<'a>> {
            check_type_input(ty).map_err(|e| {
                ExecutionError::new_with_source(
                    ExecutionErrorKind::InvalidLinkage,
                    format!("Invalid type tag in move call argument: {:?}", e),
                )
            })?;
            let ids = all_addresses(bump, ty).into_iter().map(|a| object_id(&a));
            resolution_table.add_type_linkages_to_table(ids, package_store)
        };
        match command {
            Command::MoveCall(pmc) => {
                let ProgrammableMoveCall {
                    package,
                    type_arguments,
                    ..
                } = pmc;
                type_arguments.iter().try_for_each(&mut add_ty_input)?;
                resolution_table.add_type_linkages_to_table([**package], package_store)?;
            }
            Command::MakeMoveVec(Some(ty), _) => {
                add_ty_input(ty)?;
            }
            Command::MakeMoveVec(None, _)
            | Command::TransferObjects(_, _)
            | Command::SplitCoins(_, _)
            | Command::MergeCoins(_, _) => (),
            Command::Publish(_, object_ids) => {
                resolution_table.add_type_linkages_to_table(*object_ids, package_store)?;
            }
            Command::Upgrade(_, object_ids, object_id, _) => {
                resolution_table.add_type_linkages_to_table([**object_id], package_store)?;
                resolution_table.add_type_linkages_to_table(*object_ids, package_store)?;
            }
        }

        Ok(())
    }
}
