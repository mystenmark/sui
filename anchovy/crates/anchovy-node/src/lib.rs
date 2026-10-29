// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The anchovy validator, run from a sui-node config (`validator.yaml`):
//! its keys, its genesis (`genesis.blob`), where it keeps its data, and
//! where it serves RPC and p2p. Consensus takes its addresses from the
//! committee, as sui's does. Fields anchovy has no use for are ignored.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use consensus_glue::Runner;
use fastcrypto::traits::{KeyPair as _, ToFromBytes as _};
use messages::base::Digest;
use node::epoch::{Node, Perpetual, ValidatorKeys};
use sui_config::NodeConfig;
use sui_protocol_config::Chain;
use sui_types::crypto::NetworkKeyPair;
use sui_types::digests::{ChainIdentifier, CheckpointDigest};

use sui_types::object::Object;
use tokio::runtime::Runtime;
use validator::current::CurrentEpoch;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::BuiltCheckpoint;
use validator::tls::NetworkKey;
use workqueue::Inbox;

/// How many certified checkpoints wait for state sync.
const CERTIFIED_CAPACITY: usize = 1024;

/// A genesis made by sui's genesis builder, as anchovy installs it.
pub struct Genesis {
    pub objects: Vec<Object>,
    pub checkpoint: BuiltCheckpoint,
    /// Its `CertifiedCheckpointSummary` BCS.
    pub certified: Vec<u8>,
    /// The genesis checkpoint's digest.
    pub chain_identifier: [u8; 32],
}

impl Genesis {
    pub fn from_sui(genesis: &sui_config::genesis::Genesis) -> Genesis {
        let summary = genesis.checkpoint();
        let gas = &summary.epoch_rolling_gas_cost_summary;
        let checkpoint = BuiltCheckpoint {
            seq: 0,
            summary: bcs::to_bytes(summary.data()).expect("a summary serializes"),
            digest: summary.digest().into_inner(),
            contents: bcs::to_bytes(genesis.checkpoint_contents()).expect("contents serialize"),
            network_total_transactions: summary.network_total_transactions,
            epoch_rolling_gas: [
                gas.computation_cost,
                gas.storage_cost,
                gas.storage_rebate,
                gas.non_refundable_storage_fee,
            ],
            last_height: 0,
            timestamp_ms: summary.timestamp_ms,
            root_state: None,
        };
        Genesis {
            objects: genesis.objects().to_vec(),
            chain_identifier: checkpoint.digest,
            checkpoint,
            certified: bcs::to_bytes(summary.inner()).expect("a summary serializes"),
        }
    }

    /// Installs the genesis into `store`, unless it holds a chain already.
    ///
    /// # Panics
    /// If the store cannot be written, or holds another chain.
    pub fn install(&self, store: &store::Store) {
        if store.epoch_start().expect("the store reads").is_none() {
            execution::genesis::commit(store, &self.objects).expect("genesis objects commit");
            node::epoch::genesis(
                store,
                &self.checkpoint,
                self.certified.clone(),
                self.objects.iter().map(|o| o.digest().into_inner()),
            );
        }
        let installed = store.checkpoint(0).expect("the store reads");
        assert!(
            installed == Some(self.checkpoint.encode()),
            "the store holds another chain"
        );
    }
}

/// The chain a genesis checkpoint's digest names: mainnet's, testnet's, or
/// any other.
pub fn chain(chain_identifier: [u8; 32]) -> Chain {
    ChainIdentifier::from(CheckpointDigest::new(chain_identifier)).chain()
}

/// The validator's keys, as `config` holds them.
pub fn keys(config: &NodeConfig) -> ValidatorKeys {
    ValidatorKeys {
        protocol: Arc::new(checkpoint::ProtocolKey::new(
            config.protocol_key_pair().copy(),
        )),
        worker: secret(config.worker_key_pair()),
        network: NetworkKey::from_bytes(secret(config.network_key_pair())),
    }
}

fn secret(pair: &NetworkKeyPair) -> [u8; 32] {
    pair.copy()
        .private()
        .as_bytes()
        .try_into()
        .expect("an Ed25519 secret is 32 bytes")
}

/// A running validator.
pub struct Running {
    pub store: Arc<store::Store>,
    pub current: CurrentEpoch,
    /// What the validator certified, for state sync.
    pub certified: Inbox<CertifiedCheckpoint>,
    pub p2p: node::p2p::P2p,
    pub node: tokio::task::JoinHandle<()>,
    pub rpc: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

/// Starts the validator `config` describes on `runtime`: installs its
/// genesis in a fresh database, then serves RPC and p2p and runs epoch after
/// epoch.
pub fn start(config: &NodeConfig, runtime: &Runtime) -> anyhow::Result<Running> {
    let genesis = Genesis::from_sui(config.genesis()?);
    let db = config.db_path();
    std::fs::create_dir_all(&db).with_context(|| format!("creating {}", db.display()))?;
    let store = Arc::new(open(&db.join("perpetual"))?);
    genesis.install(&store);
    let keys = keys(config);
    let chain_identifier = Digest::new(genesis.chain_identifier);
    let current = CurrentEpoch::new();
    let (state_sync, certified) = workqueue::queue(CERTIFIED_CAPACITY);
    let parameters = config
        .consensus_config
        .as_ref()
        .and_then(|c| c.parameters.clone())
        .unwrap_or_default();
    let runner = Runner::new(&keys, parameters);
    let rpc_address = config
        .network_address
        .to_socket_addr()
        .or_else(|_| config.network_address.with_zero_ip().to_socket_addr())
        .map_err(|e| anyhow::anyhow!("network address {}: {e}", config.network_address))?;
    let _runtime = runtime.enter();
    let p2p = node::p2p::start(
        config.p2p_config.listen_address,
        keys.network.secret(),
        &chain_identifier,
        current.clone(),
    )
    .context("binding the p2p address")?;
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(rpc_address))
        .with_context(|| format!("binding the RPC address {rpc_address}"))?;
    let rpc = runtime.spawn(validator::serve(
        listener,
        NetworkKey::from_bytes(keys.network.secret()),
        current.clone(),
        std::future::pending(),
    ));
    let perpetual = Perpetual {
        store: store.clone(),
        epochs: db.join("epochs"),
        current: current.clone(),
        state_sync,
        chain: chain(genesis.chain_identifier),
        chain_identifier,
        keys,
    };
    let node = runtime.spawn(Node::new(perpetual).run(runner));
    Ok(Running {
        store,
        current,
        certified,
        p2p,
        node,
        rpc,
    })
}

fn open(path: &Path) -> anyhow::Result<store::Store> {
    store::Store::open(path).map_err(|e| anyhow::anyhow!("opening {}: {e:?}", path.display()))
}
