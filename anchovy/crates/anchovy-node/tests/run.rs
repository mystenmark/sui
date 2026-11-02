// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A validator run from nothing but the sui-node config sui's network
//! builder writes for it.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use sui_protocol_config::ProtocolVersion;
use sui_swarm_config::network_config_builder::ConfigBuilder;

const TIMEOUT: Duration = Duration::from_mins(2);

/// Waits until `running` has certified checkpoint `seq`.
fn wait_for_certified(running: &anchovy_node::Running, seq: u64) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        while let Some(certified) = running.certified.try_pop() {
            if certified.seq >= seq {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for checkpoint {seq}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stops `running` as a crash would.
fn crash(runtime: &tokio::runtime::Runtime, running: anchovy_node::Running) {
    running.node.abort();
    running.rpc.abort();
    runtime.block_on(running.p2p.shutdown());
    let _ = runtime.block_on(running.node);
    let _ = runtime.block_on(running.rpc);
}

#[test]
fn a_validator_runs_from_its_sui_config() {
    let network = ConfigBuilder::new_with_temp_dir()
        .committee_size(NonZeroUsize::new(1).unwrap())
        .with_protocol_version(ProtocolVersion::new(
            execution::upgrade::MAX_PROTOCOL_VERSION,
        ))
        .build();
    let config = &network.validator_configs()[0];
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let running = anchovy_node::start(config, &runtime).unwrap();
    wait_for_certified(&running, 3);
    let store = running.store.clone();
    crash(&runtime, running);
    // The store closes once nothing holds it.
    drop(store);

    // Started again over its database, it carries on from what it
    // committed.
    let running = anchovy_node::start(config, &runtime).unwrap();
    let committed = running.store.committed_checkpoint().unwrap().unwrap();
    wait_for_certified(&running, committed + 3);
    crash(&runtime, running);
}
