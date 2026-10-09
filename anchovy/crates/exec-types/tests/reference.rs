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
