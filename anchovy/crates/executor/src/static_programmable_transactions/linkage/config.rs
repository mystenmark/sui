// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use crate::{
    data_store::{PackageMetadata, PackageStore},
    static_programmable_transactions::linkage::resolution::{
        ResolutionTable, VersionConstraint, add_and_unify,
    },
};
use containers::Bump;
use exec_types::error::ExecutionError;
use messages::base::ObjectId;
use move_binary_format::binary_config::BinaryConfig;
use move_vm_runtime::shared::types::{OriginalId, VersionId};
use sui_protocol_config::Amendments;

/// These are the set of native packages in Sui -- importantly they can be used implicitly by
/// different parts of the system and are not required to be explicitly imported always.
/// Additionally, there is no versioning concerns around these as they are "stable" for a given
/// epoch, and are the special packages that are always available, and updated in-place.
const NATIVE_PACKAGE_IDS: &[ObjectId] = &[
    ObjectId::from_u16(0x2),
    ObjectId::from_u16(0x3),
    ObjectId::from_u16(0x1),
];

/// Metadata and shared operations for the PTB linkage analysis.
pub struct ResolutionConfig_<'a> {
    /// The transaction's arena, which resolution tables live in.
    bump: &'a Bump,
    /// Config to use for the linkage analysis.
    linkage_config: LinkageConfig,
    /// Config to use for the binary analysis (needed for deserialization to determine if a
    /// function is a non-public entry function).
    binary_config: BinaryConfig,
}

impl std::fmt::Debug for ResolutionConfig_<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolutionConfig_")
            .field("linkage_config", &self.linkage_config)
            .field("binary_config", &self.binary_config)
            .finish()
    }
}

/// The reference shares the config in an `Rc`; here it is in the arena.
#[derive(Debug, Clone, Copy)]
pub struct ResolutionConfig<'a>(&'a ResolutionConfig_<'a>);

/// Configuration for the linkage analysis.
#[derive(Debug, Clone)]
pub struct LinkageConfig {
    /// Whether system packages should always be included as a member in the generated linkage.
    /// This is almost always true except for system transactions and genesis transactions.
    pub always_include_system_packages: bool,
    /// If special amendments should be included in the generated linkage.
    pub include_special_amendments: Option<Arc<Amendments>>,
}

impl<'a> ResolutionConfig<'a> {
    pub fn new(bump: &'a Bump, linkage_config: LinkageConfig, binary_config: BinaryConfig) -> Self {
        Self(containers::leak(
            bump,
            ResolutionConfig_ {
                bump,
                linkage_config,
                binary_config,
            },
        ))
    }

    pub fn bump(&self) -> &'a Bump {
        self.0.bump
    }

    pub fn linkage_config(&self) -> &LinkageConfig {
        &self.0.linkage_config
    }

    pub fn binary_config(&self) -> &BinaryConfig {
        &self.0.binary_config
    }

    pub(crate) fn resolution_table_with_native_packages<S: PackageStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<ResolutionTable<'a>, ExecutionError<'static>> {
        let mut resolution_table = ResolutionTable::empty(*self);
        if self.0.linkage_config.always_include_system_packages {
            for id in NATIVE_PACKAGE_IDS {
                #[cfg(debug_assertions)]
                {
                    use crate::static_programmable_transactions::linkage::resolution::get_package;
                    let package = get_package(id, store)?;
                    debug_assert_eq!(package.version_id(), *id);
                    debug_assert_eq!(package.original_id(), *id);
                }
                add_and_unify(id, store, &mut resolution_table, VersionConstraint::exact)?;
            }
        }

        Ok(resolution_table)
    }

    /// The reference clones the package's table for every call; this borrows it, and copies it
    /// only when amendments apply to the package.
    pub(crate) fn linkage_table<'p, P: PackageMetadata>(
        &self,
        pkg: &'p P,
    ) -> Cow<'p, BTreeMap<OriginalId, VersionId>> {
        self.linkage_config()
            .apply_linkage_amendments(pkg.version_id(), pkg.linkage_table())
    }
}

impl LinkageConfig {
    pub fn new(
        include_special_amendments: Option<Arc<Amendments>>,
        always_include_system_packages: bool,
    ) -> Self {
        Self {
            include_special_amendments,
            always_include_system_packages,
        }
    }

    fn apply_linkage_amendments<'p>(
        &self,
        root: ObjectId,
        linkage: &'p BTreeMap<OriginalId, VersionId>,
    ) -> Cow<'p, BTreeMap<OriginalId, VersionId>> {
        let Some(amendments) = &self.include_special_amendments else {
            return Cow::Borrowed(linkage);
        };

        let root: VersionId = exec_types::base::move_address(&root);
        if let Some(amendments_for_root) = amendments.get(&root) {
            let mut linkage = linkage.clone();
            for (orig_id, upgraded_id) in amendments_for_root.iter() {
                // Upgrade linkage. This can either an insert or override.
                linkage.insert(*orig_id, *upgraded_id);
            }
            return Cow::Owned(linkage);
        }
        Cow::Borrowed(linkage)
    }
}
