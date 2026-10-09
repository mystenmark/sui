// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use containers::{Bump, IndexMap, Vec};
use exec_types::error::ExecutionError;
use exec_types::object::original_package_id;
use exec_types::storage::{BackingPackageStore, SuiError, SuiResult};
use exec_types::{invariant_violation, make_invariant_violation};
use messages::base::ObjectId;
use messages::object::MovePackage;
use move_core_types::{
    account_address::AccountAddress,
    identifier::Identifier,
    resolver::{IntraPackageName, ModuleResolver, SerializedPackage},
};
use move_vm_runtime::{
    shared::types::VersionId, validation::verification::ast::Package as VerifiedPackage,
};
use std::{cell::RefCell, sync::Arc};

/// A `TransactionPackageStore` is a `ModuleResolver` that fetches packages from a backing store.
/// It also tracks packages that are being published in the current transaction and allows
/// "loading" of those packages as well.
///
/// It is used to provide package loading (from storage) for the Move VM. Packages are views,
/// borrowed from the store's messages or built in the arena, where the reference holds each in an
/// `Rc` cloned from the store's.
#[allow(clippy::type_complexity)]
pub struct TransactionPackageStore<'a> {
    bump: &'a Bump,
    package_store: &'a dyn BackingPackageStore<'a>,

    /// Elements in this are packages that are being published or have been published in the current
    /// transaction.
    /// Elements in this are _not_ safe to evict, unless evicted through `pop_package`.
    new_packages: RefCell<IndexMap<'a, ObjectId, (MovePackage<'a>, Arc<VerifiedPackage>)>>,

    /// A cache of packages that we've loaded so far. This is used to speed up package loading.
    /// Elements in this are safe to be evicted based on cache decisions.
    package_cache: RefCell<IndexMap<'a, ObjectId, Option<MovePackage<'a>>>>,
}

impl<'a> TransactionPackageStore<'a> {
    pub fn new(bump: &'a Bump, package_store: &'a dyn BackingPackageStore<'a>) -> Self {
        Self {
            bump,
            package_store,
            new_packages: RefCell::new(IndexMap::new_in(bump)),
            package_cache: RefCell::new(IndexMap::new_in(bump)),
        }
    }

    /// The transaction's arena.
    pub fn bump(&self) -> &'a Bump {
        self.bump
    }

    /// Push a new package into the new packages. This is used to track packages that are being
    /// published or have been published.
    pub fn push_package(
        &self,
        id: ObjectId,
        package: MovePackage<'a>,
        verified_package: VerifiedPackage,
    ) -> Result<(), ExecutionError<'a>> {
        // Check that the package ID is not already present anywhere.
        debug_assert!(!self.new_packages.borrow().contains_key(&id));

        // Insert the package into the new packages
        // If the package already exists, we will overwrite it and signal an error.
        if self
            .new_packages
            .borrow_mut()
            .insert(id, (package, Arc::new(verified_package)))
            .is_some()
        {
            invariant_violation!(
                "Package with ID {} already exists in the new packages. This should never happen.",
                id
            );
        }

        Ok(())
    }

    /// Rollback a package that was pushed into the new packages. We keep the invariant that:
    /// * You can only pop the most recent package that was pushed.
    /// * The element being popped _must_ exist in the new packages.
    ///
    /// Otherwise this returns an invariant violation.
    pub fn pop_package(&self, id: ObjectId) -> Result<Arc<VerifiedPackage>, ExecutionError<'a>> {
        if self
            .new_packages
            .borrow()
            .last()
            .is_none_or(|(pkg_id, _)| *pkg_id != id)
        {
            make_invariant_violation!(
                "Tried to pop package {} from new packages, but new packages was empty or \
                it is not the most recent package inserted. This should never happen.",
                id
            );
        }

        let Some((pkg_id, (_move_pkg, verified_pkg))) = self.new_packages.borrow_mut().pop() else {
            unreachable!(
                "We just checked that new packages is not empty, so this should never happen."
            );
        };
        assert_eq!(
            pkg_id, id,
            "Popped package ID {} does not match requested ID {}. This should never happen as was checked above.",
            pkg_id, id
        );

        Ok(verified_pkg)
    }

    /// Fetch a package that is being published in the current transaction, if it exists.
    /// This does not look in the backing store.
    pub fn fetch_new_package(
        &self,
        id: &ObjectId,
    ) -> Option<(MovePackage<'a>, Arc<VerifiedPackage>)> {
        self.new_packages.borrow().get(id).cloned()
    }

    /// Return all new packages that have been added to this store in the transaction.
    pub fn to_new_packages(&self, bump: &'a Bump) -> Vec<'a, MovePackage<'a>> {
        let new_packages = self.new_packages.borrow();
        let mut packages = Vec::with_capacity_in(new_packages.len(), bump);
        packages.extend(new_packages.iter().map(|(_, (move_pkg, _))| *move_pkg));
        packages
    }

    /// Fetch a package by its version ID. This will first look in the new packages, and then in
    /// the cache for any packages that have been loaded this transaction, and then in the backing store.
    /// If found, it will be returned as a [`MovePackage`].
    pub fn fetch_move_package(
        &self,
        package_version_id: VersionId,
    ) -> SuiResult<Option<MovePackage<'a>>> {
        let id = ObjectId(package_version_id.into_bytes());
        if let Some((move_pkg, _verified_pkg)) = self.fetch_new_package(&id) {
            return Ok(Some(move_pkg));
        }

        if let Some(cached_pkg) = self.package_cache.borrow().get(&id) {
            return Ok(*cached_pkg);
        }

        let move_package = self
            .package_store
            .get_package_object(&id)?
            .map(|pkg| *pkg.try_as_package().expect("a package object"));

        self.package_cache.borrow_mut().insert(id, move_package);

        Ok(move_package)
    }

    /// Fetch a package by its version ID. This will first look in the new packages, and then in
    /// the cache for any packages that have been loaded this transaction, and then in the backing store.
    /// If found, it will be returned as a [`SerializedPackage`].
    fn fetch_package(&self, package_version_id: VersionId) -> SuiResult<Option<SerializedPackage>> {
        self.fetch_move_package(package_version_id)
            .and_then(|opt_pkg| {
                opt_pkg
                    .as_ref()
                    .map(into_serialized_move_package)
                    .transpose()
            })
    }
}

/// `MovePackage::into_serialized_move_package`: the package as the VM loads it, owned, which is
/// the VM's boundary.
pub fn into_serialized_move_package(package: &MovePackage<'_>) -> SuiResult<SerializedPackage> {
    macro_rules! expect_valid_identifier {
        ($ident_str:expr) => {
            Identifier::new($ident_str).map_err(|e| {
                debug_assert!(
                    false,
                    "Published modules must always have valid identifiers {}",
                    e
                );
                SuiError("ExecutionInvariantViolation".to_owned())
            })
        };
    }
    let type_origin_table = package
        .type_origin_table
        .iter()
        .map(|ty_origin| {
            Ok((
                IntraPackageName {
                    module_name: expect_valid_identifier!(ty_origin.module_name)?,
                    type_name: expect_valid_identifier!(ty_origin.datatype_name)?,
                },
                AccountAddress::new(ty_origin.package.0),
            ))
        })
        .collect::<SuiResult<_>>()?;
    let original_id = AccountAddress::new(original_package_id(package).0);
    Ok(SerializedPackage {
        modules: package
            .module_map
            .iter()
            .map(|(k, v)| Ok((expect_valid_identifier!(*k)?, v.to_vec())))
            .collect::<SuiResult<_>>()?,
        version_id: AccountAddress::new(package.id.0),
        original_id,
        linkage_table: package
            .linkage_table
            .iter()
            .map(|l| {
                (
                    AccountAddress::new(l.original_id.0),
                    AccountAddress::new(l.upgraded_id.0),
                )
            })
            .chain(std::iter::once((
                original_id,
                AccountAddress::new(package.id.0),
            )))
            .collect(),
        type_origin_table,
        version: package.version,
    })
}

// Better days have arrived!
impl ModuleResolver for TransactionPackageStore<'_> {
    type Error = SuiError;
    fn get_packages_static<const N: usize>(
        &self,
        ids: [AccountAddress; N],
    ) -> Result<[Option<SerializedPackage>; N], Self::Error> {
        // Once https://doc.rust-lang.org/stable/std/primitive.array.html#method.try_map is stable
        // we can use that here.
        let mut packages = [const { None }; N];
        for (i, id) in ids.iter().enumerate() {
            packages[i] = self.fetch_package(*id)?;
        }

        Ok(packages)
    }

    fn get_packages<'b>(
        &self,
        ids: impl ExactSizeIterator<Item = &'b AccountAddress>,
    ) -> Result<std::vec::Vec<Option<SerializedPackage>>, Self::Error> {
        ids.map(|id| self.fetch_package(*id)).collect()
    }
}
