// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What a protocol upgrade needs of the reference's own code: this
//! validator's capabilities, whose config digests and package references
//! other validators compare byte for byte, and the system packages a chosen
//! upgrade writes (`docs/protocol_upgrade_reference.md`).

use messages::build::base as build;
use std::future::Future as _;

use move_binary_format::CompiledModule;
use sui_framework::{BuiltInFramework, SystemPackage};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, TransactionDigest};
use sui_types::crypto::AuthorityPublicKeyBytes;
use sui_types::digests::ObjectDigest;
use sui_types::messages_consensus::{AuthorityCapabilitiesV2, ConsensusTransaction};
use sui_types::object::{OBJECT_START_VERSION, Object};
use sui_types::storage::ObjectStore;
use sui_types::supported_protocol_versions::SupportedProtocolVersions;

use crate::StoreView;

/// The `CapabilityNotificationV2` a validator named `authority` submits at an
/// epoch's start, as consensus transaction BCS: the versions of `supported`
/// from the epoch's (`config`'s) on, each with its config's digest for
/// `chain`, and the system packages of this binary as they would be over
/// `store`'s. As the reference's `SuiNode::monitor_reconfiguration`, a
/// version that enables accumulators is not offered while the accumulator
/// root does not exist.
pub fn capability_notification(
    store: &store::Store,
    authority: [u8; 96],
    chain: Chain,
    config: &ProtocolConfig,
    supported: SupportedProtocolVersions,
    accumulator_root_exists: bool,
) -> Vec<u8> {
    let mut supported = supported.truncate_below(config.version);
    while supported.max > config.version
        && ProtocolConfig::get_for_version(supported.max, chain).enable_accumulators()
        && !accumulator_root_exists
    {
        supported.max = supported.max.prev();
    }
    let capabilities = AuthorityCapabilitiesV2::new(
        AuthorityPublicKeyBytes(authority),
        chain,
        supported,
        available_system_packages(store, config),
    );
    bcs::to_bytes(&ConsensusTransaction::new_capability_notification_v2(
        capabilities,
    ))
    .expect("a consensus transaction serializes")
}

/// The reference's `get_available_system_packages`: each of this binary's
/// system packages as it would be over `store`'s, or none if any is not a
/// compatible upgrade.
fn available_system_packages(store: &store::Store, config: &ProtocolConfig) -> Vec<ObjectRef> {
    let view = StoreView::new(store);
    let binary_config = config.binary_config(None);
    let mut packages = Vec::new();
    for package in BuiltInFramework::iter_system_packages() {
        let modules = package.modules();
        let compared = std::pin::pin!(sui_framework::compare_system_package(
            &view,
            &package.id,
            &modules,
            package.dependencies.clone(),
            &binary_config,
        ));
        // Async in the reference only for its object store's sake: it never
        // awaits, so one poll runs it to completion.
        let std::task::Poll::Ready(compared) =
            compared.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        else {
            unreachable!("comparing a system package never awaits");
        };
        let Some(reference) = compared else {
            return Vec::new();
        };
        packages.push(reference);
    }
    packages
}

/// A system package a `ChangeEpoch` writes: its version, module bytes and
/// dependencies.
pub type SystemPackageBytes = (build::SequenceNumber, Vec<Vec<u8>>, Vec<build::ObjectId>);

/// The reference's `get_system_package_bytes`: the packages of `chosen`
/// that change, each re-derived from this binary on top of its predecessor
/// in `store`, so that the change-epoch transaction matches every
/// validator's. None if this binary cannot make one of them.
pub fn system_package_bytes(
    store: &store::Store,
    chosen: &[build::ObjectRef],
    config: &ProtocolConfig,
) -> Option<Vec<SystemPackageBytes>> {
    let view = StoreView::new(store);
    let binary_config = config.binary_config(None);
    let mut packages = Vec::new();
    for (id, version, digest) in chosen {
        let chosen: ObjectRef = (
            ObjectID::new(id.0.0),
            SequenceNumber::from_u64(version.0),
            ObjectDigest::new(digest.0.0),
        );
        if view
            .get_object_by_key(&chosen.0, chosen.1)
            .is_some_and(|object| object.compute_object_reference() == chosen)
        {
            continue;
        }
        let previous_transaction = if chosen.1 == OBJECT_START_VERSION {
            TransactionDigest::genesis_marker()
        } else {
            view.get_object_by_key(&chosen.0, chosen.1.one_before()?)?
                .previous_transaction
        };
        let SystemPackage {
            bytes,
            dependencies,
            ..
        } = BuiltInFramework::get_package_by_id(&chosen.0).clone();
        let modules = bytes
            .iter()
            .map(|module| CompiledModule::deserialize_with_config(module, &binary_config))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let made = Object::new_system_package(
            &modules,
            chosen.1,
            dependencies.clone(),
            previous_transaction,
        );
        if made.compute_object_reference() != chosen {
            return None;
        }
        let dependencies = dependencies
            .iter()
            .map(|id| build::ObjectId(build::AccountAddress(id.into_bytes())))
            .collect();
        packages.push((build::SequenceNumber(chosen.1.value()), bytes, dependencies));
    }
    Some(packages)
}

/// The versions this binary supports.
pub fn supported_protocol_versions() -> SupportedProtocolVersions {
    SupportedProtocolVersions::SYSTEM_DEFAULT
}

/// Whether this binary can run an epoch at `version`.
pub fn is_supported(supported: SupportedProtocolVersions, version: u64) -> bool {
    supported.is_version_supported(ProtocolVersion::new(version))
}
