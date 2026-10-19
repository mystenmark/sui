// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! One anchovy validator and one sui validator, of equal stake: a quorum
//! needs both, so a checkpoint certifies only if both built it the same.
//! When they stop certifying, the test reports the first checkpoint they
//! built differently, and the effects they disagree on.

#[path = "../../node/tests/genesis/mod.rs"]
mod genesis;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use consensus_glue::{Parameters, Runner};
use fastcrypto::traits::{KeyPair as _, ToFromBytes as _};
use messages::base::Digest;
use node::epoch::{Node, Perpetual, ValidatorKeys};
use protocol_config::Chain;
use sui_config::NodeConfig;
use sui_core::authority::AuthorityState;
use sui_swarm_config::network_config_builder::ConfigBuilder;
use sui_types::crypto::NetworkKeyPair;
use sui_types::digests::TransactionDigest;
use sui_types::effects::TransactionEffects;
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointSummary,
};
use tokio::runtime::Runtime;
use validator::current::CurrentEpoch;
use validator::pipeline::items::CertifiedCheckpoint;
use workqueue::Inbox;

const TIMEOUT: Duration = Duration::from_mins(2);

/// Anchovy's node, which the test drives itself to see what it builds.
struct Anchovy {
    node: Node,
    runner: Runner,
    certified_inbox: Inbox<CertifiedCheckpoint>,
    certified: BTreeMap<u64, CertifiedCheckpointSummary>,
    // Dropped after the node.
    _dir: tempfile::TempDir,
}

struct Network {
    runtime: Runtime,
    sui: sui_swarm::memory::Node,
    anchovy: Anchovy,
}

/// The secret of an Ed25519 key pair, as anchovy holds it.
fn secret(pair: &NetworkKeyPair) -> [u8; 32] {
    pair.copy().private().as_bytes().try_into().unwrap()
}

/// Anchovy's keys: those sui's config gives the validator.
fn keys(config: &NodeConfig) -> ValidatorKeys {
    ValidatorKeys {
        protocol: Arc::new(checkpoint::ProtocolKey::new(
            config.protocol_key_pair().copy(),
        )),
        worker: secret(config.worker_key_pair()),
        network: validator::tls::NetworkKey::from_bytes(secret(config.network_key_pair())),
    }
}

impl Network {
    /// A genesis of two validators, as sui's test networks make it: the
    /// first run by sui, the second by anchovy.
    fn new() -> Network {
        telemetry_subscribers::init_for_testing();
        let config = ConfigBuilder::new_with_temp_dir()
            .committee_size(NonZeroUsize::new(2).unwrap())
            .build();
        let runtime = Runtime::new().unwrap();
        let sui = sui_swarm::memory::Node::new(config.validator_configs[0].clone());
        runtime.block_on(sui.start()).unwrap();
        let anchovy = Anchovy::start(&config.validator_configs[1], &config.genesis);
        Network {
            runtime,
            sui,
            anchovy,
        }
    }

    fn sui(&self) -> Arc<AuthorityState> {
        self.sui
            .get_node_handle()
            .expect("the sui node is running")
            .state()
    }

    /// Waits until both validators have certified checkpoint `seq`;
    /// reports where they disagree if they do not.
    fn wait_for_certified(&mut self, seq: u64) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.anchovy.poll();
            let sui = self
                .sui()
                .get_checkpoint_store()
                .get_checkpoint_by_sequence_number(seq)
                .unwrap();
            if sui.is_some() && self.anchovy.certified.contains_key(&seq) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "checkpoint {seq} did not certify\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// How far each validator got, and the first checkpoint they built
    /// differently.
    fn report(&self) -> String {
        let sui = self.sui();
        let checkpoints = sui.get_checkpoint_store();
        let sui_certified = checkpoints
            .get_highest_verified_checkpoint()
            .unwrap()
            .map(|c| *c.sequence_number());
        let mut report = format!(
            "sui: certified through {sui_certified:?}\nanchovy: certified through {:?}\n",
            self.anchovy.certified.keys().last()
        );
        for seq in 1.. {
            let built = (
                checkpoints.get_locally_computed_checkpoint(seq).unwrap(),
                self.anchovy.built(seq),
            );
            let (Some(sui_built), Some((anchovy_built, anchovy_contents))) = built else {
                writeln!(
                    report,
                    "checkpoint {seq}: built by sui: {}, by anchovy: {}",
                    built.0.is_some(),
                    built.1.is_some()
                )
                .unwrap();
                break;
            };
            if sui_built.digest() == anchovy_built.digest() {
                continue;
            }
            let sui_contents = checkpoints
                .get_checkpoint_contents(&sui_built.content_digest)
                .unwrap()
                .expect("sui keeps the contents of what it built");
            writeln!(
                report,
                "checkpoint {seq} forked\nsui: {sui_built:#?}\nanchovy: {anchovy_built:#?}"
            )
            .unwrap();
            report += &self.contents_diff(&sui, &sui_contents, &anchovy_contents);
            break;
        }
        report
    }

    /// The transactions only one side's contents list, and the effects of
    /// those both list with different effects.
    fn contents_diff(
        &self,
        sui: &AuthorityState,
        sui_contents: &CheckpointContents,
        anchovy_contents: &CheckpointContents,
    ) -> String {
        let effects = |contents: &CheckpointContents| -> BTreeMap<TransactionDigest, _> {
            contents
                .iter()
                .map(|d| (d.transaction, d.effects))
                .collect()
        };
        let (sui_listed, anchovy_listed) = (effects(sui_contents), effects(anchovy_contents));
        let mut diff = String::new();
        for (digest, sui_effects) in &sui_listed {
            match anchovy_listed.get(digest) {
                None => writeln!(diff, "only sui lists {digest}").unwrap(),
                Some(anchovy_effects) if anchovy_effects != sui_effects => {
                    let theirs = sui
                        .get_transaction_cache_reader()
                        .get_executed_effects(digest);
                    let ours = self.anchovy.effects(digest);
                    writeln!(
                        diff,
                        "{digest}: effects differ\nsui: {theirs:#?}\nanchovy: {ours:#?}"
                    )
                    .unwrap();
                }
                Some(_) => {}
            }
        }
        for digest in anchovy_listed.keys() {
            if !sui_listed.contains_key(digest) {
                writeln!(diff, "only anchovy lists {digest}").unwrap();
            }
        }
        diff
    }
}

impl Anchovy {
    fn start(config: &NodeConfig, genesis: &sui_config::genesis::Genesis) -> Anchovy {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        let genesis = genesis::from_sui(genesis, sui_types::base_types::SuiAddress::ZERO);
        execution::genesis::commit(&store, &genesis.objects).unwrap();
        node::epoch::genesis(
            &store,
            &genesis.checkpoint,
            genesis.objects.iter().map(|o| o.digest().into_inner()),
        );
        let (state_sync, certified_inbox) = workqueue::queue(1024);
        let keys = keys(config);
        let mut runner = Runner::new(
            &keys,
            Parameters {
                sync_last_known_own_block_timeout: Duration::ZERO,
                ..Parameters::default()
            },
        );
        let mut node = Node::new(Perpetual {
            store,
            epochs: dir.path().join("epochs"),
            current: CurrentEpoch::new(),
            state_sync,
            chain: Chain::Unknown,
            chain_identifier: Digest::new(genesis.chain_identifier),
            keys,
        });
        node.start_epoch(&mut runner);
        Anchovy {
            node,
            runner,
            certified_inbox,
            certified: BTreeMap::new(),
            _dir: dir,
        }
    }

    fn poll(&mut self) {
        while let Some(certified) = self.certified_inbox.try_pop() {
            let summary = bcs::from_bytes(&certified.summary).unwrap();
            self.certified.insert(certified.seq, summary);
        }
    }

    /// Checkpoint `seq` as anchovy built it, with its contents.
    fn built(&self, seq: u64) -> Option<(CheckpointSummary, CheckpointContents)> {
        let built = self.node.running()?.pipeline.checkpoints.built(seq)?;
        Some((
            bcs::from_bytes(&built.summary).unwrap(),
            bcs::from_bytes(&built.contents).unwrap(),
        ))
    }

    fn effects(&self, digest: &TransactionDigest) -> Option<TransactionEffects> {
        let executed =
            execution::executed(&self.node.perpetual.store, &digest.into_inner()).unwrap()?;
        Some(bcs::from_bytes(&executed.effects).unwrap())
    }
}

impl Drop for Anchovy {
    fn drop(&mut self) {
        node::epoch::Consensus::stop(&mut self.runner);
    }
}

#[test]
fn an_idle_network_certifies_checkpoints() {
    let mut network = Network::new();
    network.wait_for_certified(3);
    drop(network.runtime);
}
