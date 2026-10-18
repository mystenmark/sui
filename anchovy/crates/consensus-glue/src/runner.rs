// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use consensus::TransactionClient;
use consensus_config::{NetworkKeyPair, ProtocolKeyPair};
use consensus_core::{
    Clock, CommitConsumerArgs, CommitConsumerMonitor, ConsensusAuthority, NetworkType,
};
use mysten_network::Multiaddr;
use node::epoch::{Consensus, ConsensusStart, ValidatorKeys};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::verifier::Verifier;
use crate::{commits, committee, config, submit};

/// Each lane of the pipeline's transaction client, as consensus-core's
/// normal lane.
const SUBMISSION_CAPACITY: usize = 2_000;

/// Starts and stops a `ConsensusAuthority` for each epoch, as sui's
/// `ConsensusManager`. Consensus runs on the runner's own runtime; the
/// node's epoch start is synchronous and may run inside another runtime,
/// so the authority is started and stopped from a short-lived thread.
pub struct Runner {
    /// Always present; taken on drop.
    runtime: Option<Runtime>,
    protocol_key: ProtocolKeyPair,
    network_key: NetworkKeyPair,
    listen_address: Option<Multiaddr>,
    /// How many authorities have handled a commit: at 0, consensus-core
    /// recovers from amnesia.
    boot_counter: u64,
    running: Option<Running>,
}

struct Running {
    authority: ConsensusAuthority,
    monitor: Arc<CommitConsumerMonitor>,
    tasks: [JoinHandle<()>; 2],
}

impl Runner {
    /// A runner signing with `keys`, listening on `listen_address` if given,
    /// else on the committee's address for it.
    pub fn new(keys: &ValidatorKeys, listen_address: Option<Multiaddr>) -> Runner {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("consensus")
            .enable_all()
            .build()
            .expect("consensus's runtime starts");
        Runner {
            runtime: Some(runtime),
            protocol_key: committee::protocol_key_pair(&keys.worker),
            network_key: committee::network_key_pair(&keys.network.secret()),
            listen_address,
            boot_counter: 0,
            running: None,
        }
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("the runtime lives as long as the runner")
    }
}

impl Consensus for Runner {
    /// # Panics
    /// If an epoch's consensus is running.
    fn start(&mut self, start: ConsensusStart) -> TransactionClient {
        assert!(self.running.is_none(), "one epoch's consensus at a time");
        let ConsensusStart {
            epoch,
            record,
            output,
            dir,
        } = start;
        let committee = committee::committee(record.epoch, &record.committee);
        let protocol_config =
            config::protocol_config(epoch.execution.reference().protocol_config());
        let parameters = config::parameters(dir, self.listen_address.clone());
        let (args, receiver) =
            CommitConsumerArgs::new(output.replay_from.saturating_sub(1), output.last_processed);
        let monitor = args.monitor();
        let verifier = Arc::new(Verifier::new(output.votes));
        let handle = self.runtime().handle().clone();
        // Taking commits before the authority starts, as sui's handler.
        let commits = handle.spawn(commits::forward(receiver, output.commits, monitor.clone()));
        let (protocol_key, network_key) = (self.protocol_key.clone(), self.network_key.clone());
        let boot_counter = self.boot_counter;
        let authority = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    handle.block_on(ConsensusAuthority::start(
                        NetworkType::Tonic,
                        record.epoch_start_timestamp_ms,
                        committee,
                        parameters,
                        protocol_config,
                        Some(protocol_key),
                        network_key,
                        Arc::new(Clock::default()),
                        verifier,
                        None,
                        args,
                        prometheus::Registry::new(),
                        boot_counter,
                        None,
                    ))
                })
                .join()
                .expect("consensus starts")
        });
        let (client, normal, high) = TransactionClient::new(SUBMISSION_CAPACITY);
        let submissions = handle.spawn(submit::forward(
            normal,
            high,
            authority.transaction_client(),
        ));
        self.running = Some(Running {
            authority,
            monitor,
            tasks: [commits, submissions],
        });
        client
    }

    fn stop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        // As sui: an authority that handled no commit took no part in its
        // epoch, so the next still recovers.
        if running.monitor.highest_handled_commit() > 0 {
            self.boot_counter += 1;
        }
        let handle = self.runtime().handle().clone();
        std::thread::scope(|scope| {
            scope
                .spawn(|| handle.block_on(running.authority.stop()))
                .join()
                .expect("consensus stops");
        });
        for task in running.tasks {
            task.abort();
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.stop();
        // Dropping a runtime blocks, which panics inside another runtime.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
