// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use consensus::TransactionClient;
use consensus_config::{NetworkKeyPair, Parameters, ProtocolKeyPair};
use consensus_core::{
    Clock, CommitConsumerArgs, CommitConsumerMonitor, ConsensusAuthority, NetworkType,
};
use node::epoch::{Consensus, ConsensusStart, ValidatorKeys};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::verifier::Verifier;
use crate::{commits, committee, config, submit};

/// Each lane of the pipeline's transaction client, as consensus-core's
/// normal lane.
const SUBMISSION_CAPACITY: usize = 2_000;

/// Starts and stops a `ConsensusAuthority` for each epoch, as sui's
/// `ConsensusManager`, on the runner's own runtime.
pub struct Runner {
    /// Always present; taken on drop.
    runtime: Option<Runtime>,
    protocol_key: ProtocolKeyPair,
    network_key: NetworkKeyPair,
    /// Each epoch's, its database aside.
    parameters: Parameters,
    /// How many authorities have handled a commit: at 0, consensus-core
    /// recovers from amnesia.
    boot_counter: u64,
    running: Option<Running>,
}

struct Running {
    /// The authority once started, and the task submitting to it.
    authority: JoinHandle<(ConsensusAuthority, JoinHandle<()>)>,
    monitor: Arc<CommitConsumerMonitor>,
    commits: JoinHandle<()>,
}

impl Runner {
    /// A runner signing with `keys`; each epoch's authority takes
    /// `parameters`, with the epoch's database.
    pub fn new(keys: &ValidatorKeys, parameters: Parameters) -> Runner {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("consensus")
            .enable_all()
            .build()
            .expect("consensus's runtime starts");
        Runner {
            runtime: Some(runtime),
            protocol_key: committee::protocol_key_pair(&keys.worker),
            network_key: committee::network_key_pair(&keys.network.secret()),
            parameters,
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
    /// Returns at once, the authority starting in the background: the
    /// pipeline's workers start after this, and starting, the authority may
    /// wait on their votes.
    ///
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
        let parameters = Parameters {
            db_path: dir,
            ..self.parameters.clone()
        };
        let (args, receiver) =
            CommitConsumerArgs::new(output.replay_from.saturating_sub(1), output.last_processed);
        let monitor = args.monitor();
        let verifier = Arc::new(Verifier::new(output.votes));
        let runtime = self.runtime();
        // Taking commits before the authority starts, as sui's handler.
        let commits = runtime.spawn(commits::forward(receiver, output.commits, monitor.clone()));
        let (client, normal, high) = TransactionClient::new(SUBMISSION_CAPACITY);
        let authority = runtime.spawn(ConsensusAuthority::start(
            NetworkType::Tonic,
            record.epoch_start_timestamp_ms,
            committee,
            parameters,
            protocol_config,
            Some(self.protocol_key.clone()),
            self.network_key.clone(),
            Arc::new(Clock::default()),
            verifier,
            None,
            args,
            prometheus::Registry::new(),
            self.boot_counter,
            None,
        ));
        let authority = runtime.spawn(async move {
            let authority = authority.await.expect("consensus starts");
            let submissions = tokio::spawn(submit::forward(
                normal,
                high,
                authority.transaction_client(),
            ));
            (authority, submissions)
        });
        self.running = Some(Running {
            authority,
            monitor,
            commits,
        });
        client
    }

    /// Blocks until the authority has stopped. The node's epoch end may run
    /// inside another runtime, so it waits on a thread of its own.
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
                .spawn(|| {
                    handle.block_on(async {
                        let (authority, submissions) =
                            running.authority.await.expect("consensus starts");
                        authority.stop().await;
                        submissions.abort();
                    });
                })
                .join()
                .expect("consensus stops");
        });
        running.commits.abort();
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
