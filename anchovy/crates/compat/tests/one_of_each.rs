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
use messages::base::ObjectId;
use move_core_types::ident_str;
use node::epoch::{Node, Perpetual, ValidatorKeys};
use protocol_config::Chain;
use sui_config::NodeConfig;
use sui_core::authority::AuthorityState;
use sui_core::authority_client::{AuthorityAPI as _, NetworkAuthorityClient};
use sui_swarm_config::network_config_builder::ConfigBuilder;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, NetworkKeyPair};
use sui_types::digests::TransactionDigest;
use sui_types::effects::TransactionEffects;
use sui_types::effects::TransactionEffectsAPI as _;
use sui_types::gas_coin::{GAS, GasCoin};
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointSummary,
};
use sui_types::messages_grpc::SubmitTxRequest;
use sui_types::object::{Object, Owner};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, CallArg, Command, ObjectArg, SharedObjectMutability};
use sui_types::transaction::{SenderSignedData, Transaction, TransactionData};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_FRAMEWORK_PACKAGE_ID, TypeTag};
use tokio::runtime::Runtime;
use validator::Validator;
use validator::current::CurrentEpoch;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::proto::{
    RawSubmitTxRequest, RawValidatorSubmitStatus, RawWaitForEffectsRequest, SubmitTxType,
};
use validator::service::validator_server::Validator as _;
use workqueue::Inbox;

const TIMEOUT: Duration = Duration::from_mins(2);
const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

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
    sui_client: NetworkAuthorityClient,
    anchovy: Anchovy,
    /// An account genesis gave gas coins, by id.
    sender: SuiAddress,
    key: AccountKeyPair,
    coins: Vec<ObjectID>,
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
        let sui_config = &config.validator_configs[0];
        let sui = sui_swarm::memory::Node::new(sui_config.clone());
        runtime.block_on(sui.start()).unwrap();
        let sui_client = runtime
            .block_on(NetworkAuthorityClient::connect(
                &sui_config.network_address,
                sui_config.network_key_pair().public().clone(),
            ))
            .unwrap();
        let key = config.account_keys[0].copy();
        let sender = SuiAddress::from(key.public());
        let (anchovy, coins) =
            Anchovy::start(&config.validator_configs[1], &config.genesis, sender);
        Network {
            runtime,
            sui,
            sui_client,
            anchovy,
            sender,
            key,
            coins,
        }
    }

    /// Object `id`, as anchovy has it.
    fn live(&self, id: ObjectID) -> Object {
        let object = self
            .anchovy
            .node
            .perpetual
            .store
            .live_object(&ObjectId(id.into_bytes()))
            .unwrap()
            .unwrap();
        bcs::from_bytes(object.wire_bytes()).unwrap()
    }

    /// Gas coin `i`, as anchovy has it.
    fn coin(&self, i: usize) -> ObjectRef {
        self.live(self.coins[i]).compute_object_reference()
    }

    /// Shared object `id`, as an input.
    fn shared(&self, id: ObjectID, mutable: bool) -> ObjectArg {
        let Owner::Shared {
            initial_shared_version,
        } = self.live(id).owner
        else {
            panic!("{id} is shared");
        };
        ObjectArg::SharedObject {
            id,
            initial_shared_version,
            mutability: if mutable {
                SharedObjectMutability::Mutable
            } else {
                SharedObjectMutability::Immutable
            },
        }
    }

    fn sign(&self, data: TransactionData) -> Transaction {
        Transaction::from_data_and_signer(data, vec![&self.key])
    }

    /// A transaction paid with coin `i`, of the commands `f` builds.
    fn ptb(&self, i: usize, f: impl FnOnce(&mut ProgrammableTransactionBuilder)) -> Transaction {
        let mut builder = ProgrammableTransactionBuilder::new();
        f(&mut builder);
        self.sign(TransactionData::new_programmable(
            self.sender,
            vec![self.coin(i)],
            builder.finish(),
            BUDGET,
            RGP,
        ))
    }

    /// Splits 1 SUI off coin `i` and shares the new coin.
    fn share(&self, i: usize) -> Transaction {
        self.ptb(i, |b| {
            let amount = b.pure(SUI).unwrap();
            let coin = b.command(Command::SplitCoins(Argument::GasCoin, vec![amount]));
            b.programmable_move_call(
                SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("transfer").to_owned(),
                ident_str!("public_share_object").to_owned(),
                vec![TypeTag::Struct(Box::new(GasCoin::type_()))],
                vec![coin],
            );
        })
    }

    /// Splits a MIST off the shared coin `shared` to the sender, paid with
    /// coin `i`.
    fn split_shared(&self, i: usize, shared: ObjectID) -> Transaction {
        let shared = self.shared(shared, true);
        self.ptb(i, |b| {
            b.move_call(
                SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("pay").to_owned(),
                ident_str!("split").to_owned(),
                vec![GAS::type_tag()],
                vec![
                    CallArg::Object(shared),
                    CallArg::Pure(bcs::to_bytes(&1u64).unwrap()),
                ],
            )
            .unwrap();
        })
    }

    /// Reads the clock, paid with coin `i`.
    fn read_clock(&self, i: usize) -> Transaction {
        let clock = self.shared(SUI_CLOCK_OBJECT_ID, false);
        self.ptb(i, |b| {
            b.move_call(
                SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("clock").to_owned(),
                ident_str!("timestamp_ms").to_owned(),
                vec![],
                vec![CallArg::Object(clock)],
            )
            .unwrap();
        })
    }

    /// 1 SUI to a fresh address, paid with coin `i`.
    fn transfer(&self, i: usize) -> Transaction {
        self.sign(TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            self.coin(i),
            BUDGET,
            RGP,
        ))
    }

    /// Submits `transaction` through anchovy's RPC service, and waits until
    /// both validators have executed it, with the same effects, and
    /// certified its checkpoint.
    fn execute(&mut self, transaction: &Transaction) -> TransactionEffects {
        self.execute_together(&[transaction]).remove(0)
    }

    /// `execute` for `transactions` submitted in one request, so in one
    /// block.
    fn execute_together(&mut self, transactions: &[&Transaction]) -> Vec<TransactionEffects> {
        let service = Validator::over(self.anchovy.node.perpetual.current.clone());
        self.runtime.block_on(async {
            let request = RawSubmitTxRequest {
                transactions: transactions
                    .iter()
                    .map(|t| bcs::to_bytes(t).unwrap().into())
                    .collect(),
                submit_type: SubmitTxType::Default as i32,
            };
            let submitted = tokio::time::timeout(
                TIMEOUT,
                service.submit_transaction(tonic::Request::new(request)),
            )
            .await
            .expect("submitted in time")
            .unwrap();
            for (transaction, result) in transactions.iter().zip(submitted.into_inner().results) {
                let position = match result.inner.unwrap() {
                    RawValidatorSubmitStatus::Submitted(position) => Some(position),
                    RawValidatorSubmitStatus::Executed(_) => None,
                    status @ RawValidatorSubmitStatus::Rejected(_) => {
                        panic!("not submitted: {status:?}")
                    }
                };
                let request = RawWaitForEffectsRequest {
                    transaction_digest: Some(transaction.digest().into_inner().to_vec().into()),
                    consensus_position: position,
                    include_details: false,
                    ping_type: None,
                };
                tokio::time::timeout(
                    TIMEOUT,
                    service.wait_for_effects(tonic::Request::new(request)),
                )
                .await
                .expect("answered in time")
                .unwrap();
            }
        });
        self.agree_on(transactions)
    }

    /// Submits `transactions` through anchovy's RPC service in one request,
    /// without waiting for their effects; what it answered of each.
    fn submit_through_anchovy(
        &mut self,
        transactions: &[&Transaction],
    ) -> Vec<RawValidatorSubmitStatus> {
        let service = Validator::over(self.anchovy.node.perpetual.current.clone());
        self.runtime.block_on(async {
            let request = RawSubmitTxRequest {
                transactions: transactions
                    .iter()
                    .map(|t| bcs::to_bytes(t).unwrap().into())
                    .collect(),
                submit_type: SubmitTxType::Default as i32,
            };
            let submitted = tokio::time::timeout(
                TIMEOUT,
                service.submit_transaction(tonic::Request::new(request)),
            )
            .await
            .expect("submitted in time")
            .unwrap();
            submitted
                .into_inner()
                .results
                .into_iter()
                .map(|r| r.inner.unwrap())
                .collect()
        })
    }

    /// Submits `transactions` through sui's validator service, then waits as
    /// `execute_together`.
    fn execute_through_sui(&mut self, transactions: &[&Transaction]) -> Vec<TransactionEffects> {
        self.submit_through_sui(transactions);
        self.agree_on(transactions)
    }

    /// Submits `transactions` through sui's validator service, one request
    /// each, without waiting for their effects.
    fn submit_through_sui(&mut self, transactions: &[&Transaction]) {
        let client = &self.sui_client;
        self.runtime.block_on(async {
            for transaction in transactions {
                let request = SubmitTxRequest::new_transaction((*transaction).clone());
                tokio::time::timeout(TIMEOUT, client.submit_transaction(request, None))
                    .await
                    .expect("submitted in time")
                    .unwrap();
            }
        });
    }

    /// Waits until both validators have executed `transactions`, with the
    /// same effects, and certified their checkpoints; their effects.
    fn agree_on(&mut self, transactions: &[&Transaction]) -> Vec<TransactionEffects> {
        transactions
            .iter()
            .map(|transaction| {
                let seq = self.wait_for("the transaction's checkpoint", |network| {
                    network.anchovy.checkpoint_of(transaction.digest())
                });
                self.wait_for_certified(seq);
                let ours = self
                    .anchovy
                    .effects(transaction.digest())
                    .expect("executed");
                let theirs = self
                    .sui()
                    .get_transaction_cache_reader()
                    .get_executed_effects(transaction.digest())
                    .expect("executed");
                assert_eq!(ours, theirs);
                assert!(ours.status().is_ok(), "{:?}", ours.status());
                ours
            })
            .collect()
    }

    /// Waits until `f` gives something.
    fn wait_for<T>(&mut self, what: &str, mut f: impl FnMut(&Network) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.anchovy.poll();
            if let Some(t) = f(self) {
                return t;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(50));
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
        self.wait_for(&format!("checkpoint {seq} to certify"), |network| {
            let sui = network
                .sui()
                .get_checkpoint_store()
                .get_checkpoint_by_sequence_number(seq)
                .unwrap();
            (sui.is_some() && network.anchovy.certified.contains_key(&seq)).then_some(())
        });
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
                None => {
                    let transaction = sui
                        .get_transaction_cache_reader()
                        .get_transaction_block(digest)
                        .map(|t| t.data().transaction_data().clone());
                    writeln!(diff, "only sui lists {digest}: {transaction:#?}").unwrap();
                }
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
                let transaction = self.anchovy.transaction(digest);
                writeln!(diff, "only anchovy lists {digest}: {transaction:#?}").unwrap();
            }
        }
        diff
    }
}

impl Anchovy {
    /// Anchovy's node, and the gas coins genesis gave `account`.
    fn start(
        config: &NodeConfig,
        genesis: &sui_config::genesis::Genesis,
        account: SuiAddress,
    ) -> (Anchovy, Vec<ObjectID>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        let genesis = genesis::from_sui(genesis, account);
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
        let anchovy = Anchovy {
            node,
            runner,
            certified_inbox,
            certified: BTreeMap::new(),
            _dir: dir,
        };
        (anchovy, genesis.coins)
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

    /// The checkpoint anchovy built listing `transaction`.
    fn checkpoint_of(&self, transaction: &TransactionDigest) -> Option<u64> {
        (1..)
            .map_while(|seq| Some((seq, self.built(seq)?.1)))
            .find(|(_, contents)| contents.iter().any(|d| d.transaction == *transaction))
            .map(|(seq, _)| seq)
    }

    fn transaction(&self, digest: &TransactionDigest) -> Option<TransactionData> {
        let transaction = self
            .node
            .perpetual
            .store
            .transaction(&Digest::new(digest.into_inner()))
            .unwrap()?;
        let data: SenderSignedData = bcs::from_bytes(transaction.wire_bytes()).unwrap();
        Some(data.transaction_data().clone())
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
fn transfers_execute_alike() {
    let mut network = Network::new();
    let first = network.transfer(0);
    network.execute(&first);
    let again = network.transfer(0);
    let other = network.transfer(1);
    network.execute(&again);
    network.execute(&other);
    drop(network.runtime);
}

#[test]
fn shared_objects_execute_alike() {
    let mut network = Network::new();
    let share = network.share(0);
    let created = network.execute(&share).created();
    let (shared, _) = created
        .iter()
        .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
        .expect("a shared coin");
    let shared = shared.0;
    let write = network.split_shared(1, shared);
    network.execute(&write);
    let clock = network.read_clock(2);
    network.execute(&clock);
    // Both write the shared coin in one commit, if submitted together.
    let again = network.split_shared(0, shared);
    let other = network.split_shared(1, shared);
    network.execute_together(&[&again, &other]);
    drop(network.runtime);
}

#[test]
fn transactions_through_sui_execute_alike() {
    let mut network = Network::new();
    let first = network.transfer(0);
    network.execute_through_sui(&[&first]);
    let share = network.share(1);
    let created = network.execute_through_sui(&[&share]).remove(0).created();
    let (shared, _) = created
        .iter()
        .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
        .expect("a shared coin");
    let shared = shared.0;
    // A write through each validator at once.
    let ours = network.split_shared(0, shared);
    let theirs = network.split_shared(1, shared);
    network.submit_through_sui(&[&theirs]);
    network.execute(&ours);
    network.agree_on(&[&theirs]);
    drop(network.runtime);
}

#[test]
fn a_double_spend_in_one_block_executes_alike() {
    let mut network = Network::new();
    // Both pay with the same version of coin 0.
    let first = network.transfer(0);
    let second = network.transfer(0);
    let statuses = network.submit_through_anchovy(&[&first, &second]);
    assert!(
        statuses
            .iter()
            .all(|s| matches!(s, RawValidatorSubmitStatus::Submitted(_))),
        "{statuses:?}"
    );
    let winner = network.wait_for("one of them to execute", |network| {
        [&first, &second]
            .into_iter()
            .find(|t| network.anchovy.checkpoint_of(t.digest()).is_some())
            .cloned()
    });
    network.agree_on(&[&winner]);
    let later = network.transfer(1);
    network.execute(&later);
    drop(network.runtime);
}

#[test]
fn an_idle_network_certifies_checkpoints() {
    let mut network = Network::new();
    network.wait_for_certified(3);
    drop(network.runtime);
}
