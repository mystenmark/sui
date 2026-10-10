// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::data_store::cached_package_store::CachedPackageStore;
use crate::data_store::transaction_package_store::TransactionPackageStore;
use crate::layout_resolver::LayoutResolver;
use crate::static_programmable_transactions::linkage::config::{LinkageConfig, ResolutionConfig};
use crate::static_programmable_transactions::linkage::resolved_linkage::ExecutableLinkage;
use containers::Bump;
use exec_types::base::object_id;
use exec_types::object::Object;
use exec_types::storage::{BackingPackageStore, SuiError, SuiResult};
use messages::base::ObjectId;
use move_core_types::annotated_value as A;
use move_core_types::language_storage::{StructTag, TypeTag};
use move_vm_runtime::runtime::MoveRuntime;
use sui_protocol_config::ProtocolConfig;

/// Retrieve a `MoveStructLayout` from a `Type`.
///
/// The reference boxes a `TypeLayoutStore` (a `BackingPackageStore`); this borrows the store, and
/// builds each lookup's package stores in the transaction's arena.
pub struct TypeLayoutResolver<'state, 'a, 'runtime> {
    bump: &'a Bump,
    vm: &'runtime MoveRuntime,
    protocol_config: &'runtime ProtocolConfig,
    state_view: &'state dyn BackingPackageStore<'a>,
}

/// Implements BackingPackageStore traits by providing null implementations for module
/// resolution and delegating backing package resolution to the trait object.
// Also narrows the store's lifetime to a lookup's, which the store's own (invariant) type cannot.
struct NullPackageStore<'state, 'a>(&'state dyn BackingPackageStore<'a>);

impl<'state, 'a, 'runtime> TypeLayoutResolver<'state, 'a, 'runtime> {
    pub fn new(
        bump: &'a Bump,
        vm: &'runtime MoveRuntime,
        protocol_config: &'runtime ProtocolConfig,
        state_view: &'state dyn BackingPackageStore<'a>,
    ) -> Self {
        Self {
            bump,
            vm,
            protocol_config,
            state_view,
        }
    }
}

impl LayoutResolver for TypeLayoutResolver<'_, '_, '_> {
    fn get_annotated_layout(
        &mut self,
        struct_tag: &StructTag,
    ) -> Result<A::MoveDatatypeLayout, SuiError> {
        let ids = struct_tag
            .all_addresses()
            .into_iter()
            .map(|a| object_id(&a));
        let null_resolver = NullPackageStore(self.state_view);
        let resolver = CachedPackageStore::new(
            self.vm,
            TransactionPackageStore::new(self.bump, &null_resolver),
            None,
        );
        let config = ResolutionConfig::new(
            self.bump,
            LinkageConfig::new(
                self.protocol_config
                    .include_special_package_amendments_as_option()
                    .clone(),
                true,
            ),
            self.protocol_config.binary_config(None),
        );
        // The reference converts these `ExecutionError`s with `SuiError`'s `From`.
        let tag_linkage = ExecutableLinkage::type_linkage(config, ids, &resolver)
            .map_err(|e| SuiError(e.to_string()))?;
        let link_context = tag_linkage
            .linkage_context()
            .map_err(|e| SuiError(e.to_string()))?;
        let data_store = TransactionPackageStore::new(self.bump, &null_resolver);
        let Ok(vm) = self.vm.make_vm(data_store, link_context) else {
            return Err(fail_object_layout(struct_tag));
        };

        let type_tag = TypeTag::Struct(Box::new(struct_tag.clone()));
        match vm.annotated_type_layout(&type_tag) {
            Ok(A::MoveTypeLayout::Struct(s)) => Ok(A::MoveDatatypeLayout::Struct(s)),
            Ok(A::MoveTypeLayout::Enum(e)) => Ok(A::MoveDatatypeLayout::Enum(e)),
            _ => Err(fail_object_layout(struct_tag)),
        }
    }
}

/// `SuiErrorKind::FailObjectLayout`.
fn fail_object_layout(struct_tag: &StructTag) -> SuiError {
    SuiError(format!(
        "FailObjectLayout: Fail to retrieve Object layout for {struct_tag}"
    ))
}

impl<'l, 'state: 'l, 'a: 'state> BackingPackageStore<'l> for NullPackageStore<'state, 'a> {
    fn get_package_object(&self, package_id: &ObjectId) -> SuiResult<Option<Object<'l>>> {
        self.0.get_package_object(package_id)
    }
}
