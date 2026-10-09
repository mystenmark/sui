// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Id derivation and `TxContext`'s Move encoding against sui-types.

use exec_types::base::derive_id;
use exec_types::tx_context::TxContext;
use messages::base::{Digest, SuiAddress};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};

#[test]
fn derived_ids_match() {
    for (digest, n) in [([0u8; 32], 0), ([7; 32], 1), ([0xab; 32], u64::MAX)] {
        let ours = derive_id(&Digest::new(digest), n);
        let theirs = sui_types::base_types::ObjectID::derive_id(
            sui_types::digests::TransactionDigest::new(digest),
            n,
        );
        assert_eq!(ours.0, theirs.into_bytes());
    }
}

#[test]
fn legacy_contexts_match() {
    let mut config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    for native in [true, false] {
        config.set_move_native_context_for_testing(native);
        let (sender, digest, sponsor) = ([3u8; 32], [9u8; 32], Some([4u8; 32]));
        let mut ours = TxContext::new_from_components(
            &SuiAddress(sender),
            &Digest::new(digest),
            &7,
            1234,
            1000,
            1500,
            50_000_000,
            sponsor.map(SuiAddress),
            &config,
        );
        let mut theirs = sui_types::base_types::TxContext::new_from_components(
            &sui_types::base_types::SuiAddress::from_bytes(sender).unwrap(),
            &sui_types::digests::TransactionDigest::new(digest),
            &7,
            1234,
            1000,
            1500,
            50_000_000,
            sponsor.map(|s| sui_types::base_types::SuiAddress::from_bytes(s).unwrap()),
            &config,
        );
        for _ in 0..3 {
            assert_eq!(ours.fresh_id().0, theirs.fresh_id().into_bytes());
            assert_eq!(
                ours.to_bcs_legacy_context().to_vec(),
                theirs.to_bcs_legacy_context(),
                "native {native}"
            );
        }
    }
}

#[test]
fn object_types_match() {
    use std::str::FromStr;

    use move_core_types::language_storage::StructTag;
    let tags = [
        "0x2::coin::Coin<0x2::sui::SUI>",
        "0x2::coin::Coin<0xabc::usdc::USDC>",
        "0x2::coin::Coin<vector<u8>>",
        "0x3::staking_pool::StakedSui",
        "0x2::dynamic_field::Field<0x2::accumulator::Key<0x2::balance::Balance<0x2::sui::SUI>>, 0x2::accumulator::U128>",
        "0x2::dynamic_field::Field<0x2::accumulator::Key<0x2::balance::Balance<0x7::t::T>>, 0x2::accumulator::U128>",
        "0x2::dynamic_field::Field<0x2::accumulator::Key<0x7::t::T>, 0x2::accumulator::U128>",
        "0x2::dynamic_field::Field<u64, vector<0x2::object::ID>>",
        "0x2::balance::Balance<0x2::sui::SUI>",
        "0xdee9::clob::Pool<0x2::sui::SUI, 0x5::x::Y<u128, address>>",
    ];
    let bump = containers::Bump::with_capacity(1 << 16);
    for tag in tags {
        let tag = StructTag::from_str(tag).unwrap();
        let ours = exec_types::type_tags::move_object_type_in(&bump, &tag);
        let theirs = sui_types::base_types::MoveObjectType::from(tag.clone());
        let mut w = messages::fast::Writer::new_in(&bump, 256);
        w.move_object_type(&ours);
        assert_eq!(w.finish_bytes(), bcs::to_bytes(&theirs).unwrap(), "{tag}");
        assert_eq!(ours.bcs_size(), bcs::to_bytes(&theirs).unwrap().len());
        // From a view of the tag, and back to the full tag as a view.
        let of_view = exec_types::type_tags::move_object_type_of(
            &exec_types::type_tags::struct_tag_in(&bump, &tag),
        );
        let mut w = messages::fast::Writer::new_in(&bump, 256);
        w.move_object_type(&of_view);
        assert_eq!(w.finish_bytes(), bcs::to_bytes(&theirs).unwrap(), "{tag}");
        let full = exec_types::type_tags::move_object_type_struct_tag_in(&bump, &ours);
        let mut w = messages::fast::Writer::new_in(&bump, 256);
        w.struct_tag(&full);
        assert_eq!(
            w.finish_bytes(),
            bcs::to_bytes(&StructTag::from(theirs.clone())).unwrap(),
            "{tag}"
        );
        assert_eq!(
            exec_types::type_tags::to_move_struct_tag_of(&ours),
            StructTag::from(theirs),
            "{tag}"
        );
    }
}

#[test]
fn object_type_addresses_match() {
    use std::str::FromStr;

    use move_core_types::language_storage::StructTag;
    let tags = [
        "0x2::coin::Coin<0x2::sui::SUI>",
        "0x2::coin::Coin<0xabc::usdc::USDC>",
        "0x3::staking_pool::StakedSui",
        "0x2::dynamic_field::Field<0x2::accumulator::Key<0x2::balance::Balance<0x2::sui::SUI>>, 0x2::accumulator::U128>",
        "0x2::dynamic_field::Field<0x2::accumulator::Key<0x2::balance::Balance<0x7::t::T<0x9::a::B>>>, 0x2::accumulator::U128>",
        "0xdee9::clob::Pool<0x2::sui::SUI, 0x5::x::Y<u128, 0x6::z::Z>>",
    ];
    let bump = containers::Bump::with_capacity(1 << 16);
    for tag in tags {
        let tag = StructTag::from_str(tag).unwrap();
        let ours = exec_types::type_tags::move_object_type_in(&bump, &tag);
        let theirs: Vec<_> = tag.all_addresses().into_iter().collect();
        let ours: Vec<_> = exec_types::type_tags::move_object_type_all_addresses(&bump, &ours)
            .into_iter()
            .collect();
        assert_eq!(ours, theirs, "{tag}");
    }
}

#[test]
fn package_digests_match() {
    let modules: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![9; 40], vec![]];
    let deps = [[7u8; 32], [1u8; 32]];
    let bump = containers::Bump::with_capacity(1 << 12);
    for hash_modules in [true, false] {
        let ours = exec_types::object::compute_digest_for_modules_and_deps(
            &bump,
            &modules.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            &deps.map(messages::base::ObjectId),
            hash_modules,
        );
        let theirs = sui_types::move_package::MovePackage::compute_digest_for_modules_and_deps(
            &modules,
            &deps.map(sui_types::base_types::ObjectID::new),
            hash_modules,
        );
        assert_eq!(ours, theirs);
    }
}
