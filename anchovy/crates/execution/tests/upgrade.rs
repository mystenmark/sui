// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The protocol versions anchovy announces: only those its executor runs.

use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};

#[test]
fn only_versions_the_executor_runs_are_supported() {
    for chain in [Chain::Unknown, Chain::Testnet, Chain::Mainnet] {
        let supported = execution::upgrade::supported_protocol_versions(chain);
        assert_eq!(supported.max, ProtocolVersion::MAX);
        for v in supported.as_range() {
            let config = ProtocolConfig::get_for_version(ProtocolVersion::new(v), chain);
            assert!(executor::runs(&config), "{chain:?} version {v}");
        }
        let below = ProtocolConfig::get_for_version(supported.min.prev(), chain);
        assert!(!executor::runs(&below), "{chain:?} below {:?}", supported.min);
    }
}
