// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    data_store::{PackageMetadata, PackageStore},
    static_programmable_transactions::linkage::config::ResolutionConfig,
};
use containers::{BTreeMap, btree_map::Entry};
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use messages::base::ObjectId;
use std::borrow::Borrow;

/// Unifiers. These are used to determine how to unify two packages.
#[derive(Debug, Clone, Copy)]
pub enum VersionConstraint {
    /// An exact constraint unifies as follows:
    /// 1. Exact(a) ~ Exact(b) ==> Exact(a), iff a == b
    /// 2. Exact(a) ~ AtLeast(b) ==> Exact(a), iff a >= b
    Exact(u64, ObjectId),
    /// An at least constraint unifies as follows:
    /// * AtLeast(a, a_version) ~ AtLeast(b, b_version) ==> AtLeast(x, max(a_version, b_version)),
    ///   where x is the package id of either a or b (the one with the greatest version).
    AtLeast(u64, ObjectId),
}

/// How a specific version of a package resolves, recorded for every package version the linkage
/// refinement touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageResolution {
    /// The original id of the package.
    pub original_id: ObjectId,
    /// The resolved version of the package. `None` only for the late-bound self entry that
    /// `ResolvedLinkage::update_for_publication` adds.
    pub version: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ResolutionTable<'a> {
    pub(crate) config: ResolutionConfig<'a>,
    pub(crate) resolution_table: BTreeMap<'a, ObjectId, VersionConstraint>,
    /// For every version of every package that we have seen, a mapping of the ObjectID for that
    /// package to its runtime ID.
    pub(crate) all_versions_resolution_table: BTreeMap<'a, ObjectId, PackageResolution>,
}

impl<'a> ResolutionTable<'a> {
    pub fn empty(config: ResolutionConfig<'a>) -> Self {
        Self {
            config,
            resolution_table: BTreeMap::new_in(config.bump()),
            all_versions_resolution_table: BTreeMap::new_in(config.bump()),
        }
    }

    /// Given a list of object IDs, generate a `ResolvedLinkage` for them.
    /// Since this linkage analysis should only be used for types, all packages are resolved
    /// "upwards" (i.e., later versions of the package are preferred).
    pub fn add_type_linkages_to_table<I, S>(
        &mut self,
        ids: I,
        store: &S,
    ) -> Result<(), ExecutionError<'static>>
    where
        S: PackageStore + ?Sized,
        I: IntoIterator,
        I::Item: Borrow<ObjectId>,
    {
        for id in ids {
            let pkg = get_package(id.borrow(), store)?;
            let package_id = pkg.version_id();
            // The reference fetches `package_id` again; when it is the ID just fetched, that
            // fetch returns `pkg`, as nothing is published in between.
            if package_id == *id.borrow() {
                add_and_unify_package(&package_id, &pkg, store, self, VersionConstraint::at_least)?;
            } else {
                add_and_unify(&package_id, store, self, VersionConstraint::at_least)?;
            }
            for object_id in self.config.linkage_table(&pkg).values() {
                let object_id = ObjectId(object_id.into_bytes());
                add_and_unify(&object_id, store, self, VersionConstraint::at_least)?;
            }
        }
        Ok(())
    }
}

impl VersionConstraint {
    pub(crate) fn object_id(&self) -> ObjectId {
        match self {
            VersionConstraint::Exact(_, id) | VersionConstraint::AtLeast(_, id) => *id,
        }
    }

    pub(crate) fn exact<P: PackageMetadata>(pkg: &P) -> Option<VersionConstraint> {
        Some(VersionConstraint::Exact(pkg.version(), pkg.version_id()))
    }

    pub(crate) fn at_least<P: PackageMetadata>(pkg: &P) -> Option<VersionConstraint> {
        Some(VersionConstraint::AtLeast(pkg.version(), pkg.version_id()))
    }

    pub fn unify(
        &self,
        other: &VersionConstraint,
    ) -> Result<VersionConstraint, ExecutionError<'static>> {
        match (&self, other) {
            // If we have two exact resolutions, they must be the same.
            (VersionConstraint::Exact(sv, self_id), VersionConstraint::Exact(ov, other_id)) => {
                if self_id != other_id || sv != ov {
                    Err(ExecutionError::new_with_source(
                        ExecutionErrorKind::InvalidLinkage,
                        format!(
                            "exact/exact conflicting resolutions for package: linkage requires the same package \
                                 at different versions. Linkage requires exactly {self_id} (version {sv}) and \
                                 {other_id} (version {ov}) to be used in the same transaction"
                        ),
                    ))
                } else {
                    Ok(VersionConstraint::Exact(*sv, *self_id))
                }
            }
            // Take the max if you have two at least resolutions.
            (
                VersionConstraint::AtLeast(self_version, sid),
                VersionConstraint::AtLeast(other_version, oid),
            ) => {
                let id = if self_version > other_version {
                    *sid
                } else {
                    *oid
                };

                Ok(VersionConstraint::AtLeast(
                    *self_version.max(other_version),
                    id,
                ))
            }
            // If you unify an exact and an at least, the exact must be greater than or equal to
            // the at least. It unifies to an exact.
            (
                VersionConstraint::Exact(exact_version, exact_id),
                VersionConstraint::AtLeast(at_least_version, at_least_id),
            )
            | (
                VersionConstraint::AtLeast(at_least_version, at_least_id),
                VersionConstraint::Exact(exact_version, exact_id),
            ) => {
                if exact_version < at_least_version {
                    return Err(ExecutionError::new_with_source(
                        ExecutionErrorKind::InvalidLinkage,
                        format!(
                            "Exact/AtLeast conflicting resolutions for package: linkage requires exactly this \
                                 package {exact_id} (version {exact_version}) and also at least the following \
                                 version of the package {at_least_id} at version {at_least_version}. However \
                                 {exact_id} is at version {exact_version} which is less than {at_least_version}."
                        ),
                    ));
                }

                Ok(VersionConstraint::Exact(*exact_version, *exact_id))
            }
        }
    }
}

/// Load a package from the store, and update the type origin map with the types in that
/// package.
pub(crate) fn get_package<S: PackageStore + ?Sized>(
    object_id: &ObjectId,
    store: &S,
) -> Result<S::Package, ExecutionError<'static>> {
    store
        .get_package(object_id)
        .map_err(|e| {
            ExecutionError::new_with_source(ExecutionErrorKind::PublishUpgradeMissingDependency, e)
        })?
        .ok_or_else(|| ExecutionError::from_kind(ExecutionErrorKind::InvalidLinkage))
}

// Add a package to the unification table, unifying it with any existing package in the table.
// Errors if the packages cannot be unified (e.g., if one is exact and the other is not).
pub(crate) fn add_and_unify<S: PackageStore + ?Sized>(
    object_id: &ObjectId,
    store: &S,
    resolution_table: &mut ResolutionTable<'_>,
    resolution_fn: fn(&S::Package) -> Option<VersionConstraint>,
) -> Result<(), ExecutionError<'static>> {
    let package = get_package(object_id, store)?;
    add_and_unify_package(object_id, &package, store, resolution_table, resolution_fn)
}

/// `add_and_unify` with `package`, already fetched by the caller as `object_id`. The reference
/// fetches it again; no package is published or rolled back in between, so the store returns the
/// same package for the same ID.
pub(crate) fn add_and_unify_package<S: PackageStore + ?Sized>(
    object_id: &ObjectId,
    package: &S::Package,
    store: &S,
    resolution_table: &mut ResolutionTable<'_>,
    resolution_fn: fn(&S::Package) -> Option<VersionConstraint>,
) -> Result<(), ExecutionError<'static>> {
    debug_assert!(get_package(object_id, store).is_ok_and(|p| {
        p.version_id() == package.version_id()
            && p.original_id() == package.original_id()
            && p.version() == package.version()
    }));

    let Some(resolution) = resolution_fn(package) else {
        // If the resolution function returns None, we do not need to add this package to the
        // resolution table, and this does not contribute to the linkage analysis.
        return Ok(());
    };
    let original_pkg_id = package.original_id();

    if let Entry::Vacant(e) = resolution_table.resolution_table.entry(original_pkg_id) {
        e.insert(resolution);
    } else {
        let existing_unifier = resolution_table
            .resolution_table
            .get_mut(&original_pkg_id)
            .expect("Guaranteed to exist");
        *existing_unifier = existing_unifier.unify(&resolution)?;
    }

    if !resolution_table
        .all_versions_resolution_table
        .contains_key(object_id)
    {
        resolution_table.all_versions_resolution_table.insert(
            *object_id,
            PackageResolution {
                original_id: original_pkg_id,
                version: Some(package.version()),
            },
        );
    }

    Ok(())
}
