// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Package building for publish and upgrade, and the upgrade compatibility check, against
//! sui-types' and the reference adapter's: the same modules and dependencies through both,
//! comparing the package object's bytes and digest, or the error kind. Also event digests.

use std::str::FromStr;

mod common;

use common::*;
use containers::Bump;
use exec_types::error::ExecutionError;
use move_binary_format::CompiledModule;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::StructTag;
use sui_framework::BuiltInFramework;
use sui_protocol_config::ProtocolConfig;
use sui_types::base_types::{ObjectID, TransactionDigest};
use sui_types::execution_status::{ExecutionFailure, ExecutionStatus};
use sui_types::move_package::{MovePackage, UpgradePolicy};
use sui_types::object::Object;

fn config() -> ProtocolConfig {
    ProtocolConfig::get_for_max_version_UNSAFE()
}

fn framework_modules(id: u8) -> Vec<CompiledModule> {
    BuiltInFramework::get_package_by_id(&ObjectID::from_single_byte(id)).modules()
}

/// The package view of a sui-types package, through its stored object.
fn package_view<'a>(bump: &'a Bump, package: &MovePackage) -> messages::object::MovePackage<'a> {
    let object = Object::new_from_package(package.clone(), TransactionDigest::genesis_marker());
    *stored_object(bump, &object).try_as_package().unwrap()
}

fn assert_same_kind(
    bump: &Bump,
    ours: &ExecutionError<'_>,
    theirs: &sui_types::error::ExecutionError,
) {
    let theirs = status(
        bump,
        &ExecutionStatus::Failure(ExecutionFailure {
            error: theirs.kind().clone(),
            command: None,
        }),
    );
    let ours = messages::execution_status::ExecutionStatus::Failure {
        error: *ours.kind(),
        command: None,
    };
    assert_eq!(ours, theirs);
}

/// Both built the same package, or failed with the same kind; the built package, if any.
fn assert_same_package<'a>(
    bump: &'a Bump,
    ours: Result<messages::object::MovePackage<'a>, ExecutionError<'a>>,
    theirs: Result<MovePackage, sui_types::error::ExecutionError>,
) -> Option<(messages::object::MovePackage<'a>, MovePackage)> {
    match (ours, theirs) {
        (Ok(ours), Ok(theirs)) => {
            let previous = TransactionDigest::new([5; 32]);
            let ours_object =
                exec_types::object::Object::new_from_package(ours, digest(previous)).seal(bump);
            let theirs_object = Object::new_from_package(theirs.clone(), previous);
            assert_eq!(
                ours_object.stored_bytes().unwrap(),
                bcs::to_bytes(&theirs_object).unwrap()
            );
            assert_eq!(ours_object.digest(), digest(theirs_object.digest()));
            assert_eq!(
                exec_types::object::move_package_size(&ours),
                theirs.size(),
                "size"
            );
            Some((ours, theirs))
        }
        (Err(ours), Err(theirs)) => {
            assert_same_kind(bump, &ours, &theirs);
            None
        }
        (ours, theirs) => panic!(
            "outcomes differ: ours {:?}, theirs {:?}",
            ours.map(|p| *p.id),
            theirs.map(|p| p.id())
        ),
    }
}

#[test]
fn initial_packages_match() {
    let config = config();
    let bump = Bump::with_capacity(1 << 24);
    let std_package =
        BuiltInFramework::get_package_by_id(&ObjectID::from_single_byte(1)).genesis_move_package();
    let std_view = package_view(&bump, &std_package);
    let std_modules = framework_modules(1);
    let sui_modules = framework_modules(2);

    // The standard library, which has no dependencies.
    assert!(
        assert_same_package(
            &bump,
            executor::move_package::new_initial(&bump, &std_modules, &config, []),
            MovePackage::new_initial(&std_modules, &config, []),
        )
        .is_some()
    );

    // Prefixes of the framework's modules over the standard library: module names of different
    // lengths, so the encoded order is not the name order. The whole framework is too big.
    let (mut built, mut missing) = (0, 0);
    for n in [1, 2, 5, 10, 20, sui_modules.len()] {
        let modules = &sui_modules[..n];
        built += usize::from(
            assert_same_package(
                &bump,
                executor::move_package::new_initial(&bump, modules, &config, [&std_view]),
                MovePackage::new_initial(modules, &config, [&std_package]),
            )
            .is_some(),
        );

        // Without the standard library, a dependency is missing (unless no module uses it).
        missing += usize::from(
            assert_same_package(
                &bump,
                executor::move_package::new_initial(&bump, modules, &config, []),
                MovePackage::new_initial(modules, &config, []),
            )
            .is_none(),
        );

        // A dependency given twice.
        assert_same_package(
            &bump,
            executor::move_package::new_initial(&bump, modules, &config, [&std_view, &std_view]),
            MovePackage::new_initial(modules, &config, [&std_package, &std_package]),
        );
    }
    assert!(built >= 4, "only {built} packages built");
    assert!(missing >= 3, "only {missing} missing dependencies");
    let too_big = executor::move_package::new_initial(&bump, &sui_modules, &config, [&std_view]);
    assert!(matches!(
        too_big.map(|_| ()).unwrap_err().kind(),
        messages::execution_status::ExecutionErrorKind::MovePackageTooBig { .. }
    ));
}

#[test]
fn upgraded_packages_match() {
    let config = config();
    let bump = Bump::with_capacity(1 << 24);
    let mut rng = Rng(0x5eed);
    let std_package =
        BuiltInFramework::get_package_by_id(&ObjectID::from_single_byte(1)).genesis_move_package();
    let std_view = package_view(&bump, &std_package);
    let sui_modules = framework_modules(2);

    let predecessor =
        MovePackage::new_initial(&sui_modules[..10], &config, [&std_package]).unwrap();
    let predecessor_view = package_view(&bump, &predecessor);

    let mut outcomes = (0, 0);
    // The same modules, more modules, and fewer (which drops types the predecessor defines).
    for modules in [&sui_modules[..10], &sui_modules[..15], &sui_modules[..5]] {
        let storage_id = rng.id();
        let built = assert_same_package(
            &bump,
            executor::move_package::new_upgraded(
                &bump,
                &predecessor_view,
                oid(storage_id),
                modules,
                &config,
                [&std_view],
            ),
            predecessor.new_upgraded(storage_id, modules, &config, [&std_package]),
        );
        if built.is_some() {
            outcomes.0 += 1;
        } else {
            outcomes.1 += 1;
        }
    }
    assert_eq!(outcomes, (2, 1));

    // An upgrade of an upgrade: type origins carry over from the first version.
    let upgraded_id = rng.id();
    let upgraded = predecessor
        .new_upgraded(upgraded_id, &sui_modules[..15], &config, [&std_package])
        .unwrap();
    let upgraded_view = package_view(&bump, &upgraded);
    let storage_id = rng.id();
    assert!(
        assert_same_package(
            &bump,
            executor::move_package::new_upgraded(
                &bump,
                &upgraded_view,
                oid(storage_id),
                &sui_modules[..20],
                &config,
                [&std_view],
            ),
            upgraded.new_upgraded(storage_id, &sui_modules[..20], &config, [&std_package]),
        )
        .is_some()
    );
}

#[test]
fn compatibility_checks_match() {
    let config = config();
    let bump = Bump::with_capacity(1 << 24);
    let std_package =
        BuiltInFramework::get_package_by_id(&ObjectID::from_single_byte(1)).genesis_move_package();
    let sui_modules = framework_modules(2);
    let existing = MovePackage::new_initial(&sui_modules[..10], &config, [&std_package]).unwrap();
    let existing_view = package_view(&bump, &existing);

    let mut outcomes = (0, 0);
    for modules in [&sui_modules[..10], &sui_modules[..12], &sui_modules[..9]] {
        for policy in [
            UpgradePolicy::COMPATIBLE,
            UpgradePolicy::ADDITIVE,
            UpgradePolicy::DEP_ONLY,
            7,
        ] {
            let ours =
                executor::static_programmable_transactions::execution::context::check_compatibility(
                    &bump,
                    &config,
                    &existing_view,
                    modules,
                    policy,
                );
            let theirs =
                sui_adapter::static_programmable_transactions::execution::context::check_compatibility(
                    &config, &existing, modules, policy,
                );
            match (ours, theirs) {
                (Ok(()), Ok(())) => outcomes.0 += 1,
                (Err(ours), Err(theirs)) => {
                    assert_same_kind(&bump, &ours, &theirs);
                    outcomes.1 += 1;
                }
                (ours, theirs) => panic!("policy {policy}: ours {ours:?}, theirs {theirs:?}"),
            }
        }
    }
    // Same modules pass every known policy, added modules pass all but deps-only; removed modules
    // and the unknown policy fail.
    assert_eq!(outcomes, (5, 7));
}

#[test]
fn event_digests_match() {
    let bump = Bump::with_capacity(1 << 16);
    let mut rng = Rng(0xe7e7);
    for (module, tag, len) in [
        ("m", "0x2::coin::CoinMetadata<0x2::sui::SUI>", 0),
        ("event", "0xabc::a_module::E", 17),
        ("x", "0x7::t::T<u8, vector<0x8::u::U<address>>>", 300),
    ] {
        let contents: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let theirs = sui_types::event::Event {
            package_id: rng.id(),
            transaction_module: Identifier::new(module).unwrap(),
            sender: rng.address(),
            type_: StructTag::from_str(tag).unwrap(),
            contents: contents.clone(),
        };
        let ours = messages::effects::Event {
            package_id: containers::alloc(&bump, oid(theirs.package_id)),
            transaction_module: module,
            sender: containers::alloc(&bump, messages::base::SuiAddress(theirs.sender.to_inner())),
            type_: exec_types::type_tags::struct_tag_in(&bump, &theirs.type_),
            contents: &contents,
        };
        assert_eq!(
            messages::fast::event_digest(&bump, &ours),
            digest(theirs.digest())
        );
    }
}
