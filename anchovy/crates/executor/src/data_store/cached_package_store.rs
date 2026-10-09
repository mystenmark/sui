// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::data_store::{PackageStore, transaction_package_store::TransactionPackageStore};
use containers::IndexMap;
use exec_types::storage::{SuiError, SuiResult};
use messages::base::ObjectId;
use messages::object::MovePackage;
use move_core_types::{
    account_address::AccountAddress, identifier::IdentStr, resolver::IntraPackageName,
};
use move_vm_runtime::{
    cache::move_cache::ResolvedPackageResult, runtime::MoveRuntime,
    validation::verification::ast::Package as VerifiedPackage,
};
use std::cell::RefCell;
use std::sync::Arc;

/// The `CachedPackageStore` is a `PackageStore` implementation that uses a `MoveRuntime` to
/// fetch and cache packages. It also uses an underlying `TransactionPackageStore` to fetch packages
/// that are not in the cache. This is used to provide package loading (from storage)
/// for the Move VM, while also allowing for packages that are being published in the
/// current transaction to be found.
pub struct CachedPackageStore<'state, 'runtime> {
    /// The Move runtime to use for fetching and caching packages.
    runtime: &'runtime MoveRuntime,

    /// Underlying store to fetch packages from. Any newly published packages in the current
    /// transaction should be in the `new_packages` field of this store.
    pub package_store: TransactionPackageStore<'state>,

    /// Packages the runtime resolved for this store, by package (storage) ID. Not in the
    /// reference, which goes to the runtime on every lookup; a transaction looks up the same few
    /// packages dozens of times, and each runtime lookup costs allocations and timers even when
    /// it hits the runtime's cache.
    ///
    /// A hit is the runtime's answer for the same ID: a package ID names one immutable version
    /// (publishing and upgrading make new IDs; system packages upgraded at an epoch change are
    /// loaded by a new runtime, with a new store). Only packages found are kept, so a package
    /// published later in the transaction is never shadowed; packages published in the
    /// transaction are looked up before this, as before.
    resolved: RefCell<IndexMap<'state, ObjectId, Arc<VerifiedPackage>>>,
}

impl<'state, 'runtime> CachedPackageStore<'state, 'runtime> {
    pub fn new(
        runtime: &'runtime MoveRuntime,
        package_store: TransactionPackageStore<'state>,
    ) -> Self {
        Self {
            runtime,
            resolved: RefCell::new(IndexMap::new_in(package_store.bump())),
            package_store,
        }
    }

    /// Get a package by its package ID (i.e., not original ID). This will first look in the new
    /// packages, and then fetch the pacakge from the underlying Move runtime which handles loading
    /// and caching of packages. If the package is not found, None is returned. If there is an error
    /// fetching the package, an error is returned.
    ///
    /// Once the package is fetched it is in the Move runtime cache, and will be found there on
    /// subsequent lookups.
    pub fn get_package(&self, object_id: &ObjectId) -> SuiResult<Option<Arc<VerifiedPackage>>> {
        self.fetch_package(object_id)
    }

    pub fn get_move_package(&self, object_id: &ObjectId) -> SuiResult<Option<MovePackage<'state>>> {
        self.package_store
            .fetch_move_package(AccountAddress::new(object_id.0))
    }

    /// Get a package by its package ID (i.e., not original ID). This will first look in the new
    /// packages, and then fetch the pacakge from the underlying Move runtime which handles loading
    /// and caching of packages.
    fn fetch_package(&self, id: &ObjectId) -> SuiResult<Option<Arc<VerifiedPackage>>> {
        // Look for package in new packages first. If we have just published the package we are
        // looking up it may not be in the VM runtime cache yet, and we don't want to add it to the
        // cache either. So if it's in the new packages, we return it directly.
        if let Some((_move_pkg, verified_pkg)) = self.package_store.fetch_new_package(id) {
            return Ok(Some(verified_pkg));
        }

        if let Some(pkg) = self.resolved.borrow().get(id) {
            return Ok(Some(pkg.clone()));
        }

        // load the package via the Move runtime, which will cache it if found.
        match self
            .runtime
            .resolve_and_cache_package(&self.package_store, AccountAddress::new(id.0))
            .map_err(|e| {
                // The reference wraps the VM's message in a `VMVerificationOrDeserializationError`
                // execution error's.
                SuiError(format!("VMVerificationOrDeserializationError: {e}"))
            })? {
            ResolvedPackageResult::Found(pkg) => {
                self.resolved.borrow_mut().insert(*id, pkg.verified.clone());
                Ok(Some(pkg.verified.clone()))
            }
            ResolvedPackageResult::NotFound => Ok(None),
        }
    }
}

impl PackageStore for CachedPackageStore<'_, '_> {
    type Package = Arc<VerifiedPackage>;

    fn get_package(&self, id: &ObjectId) -> SuiResult<Option<Self::Package>> {
        self.get_package(id)
    }

    fn resolve_type_to_defining_id(
        &self,
        module_address: ObjectId,
        module_name: &IdentStr,
        type_name: &IdentStr,
    ) -> SuiResult<Option<ObjectId>> {
        let Some(pkg) = self.get_package(&module_address)? else {
            return Ok(None);
        };

        Ok(pkg
            .type_origin_table()
            .get(&IntraPackageName {
                module_name: module_name.to_owned(),
                type_name: type_name.to_owned(),
            })
            .map(|id| ObjectId(id.into_bytes())))
    }
}
