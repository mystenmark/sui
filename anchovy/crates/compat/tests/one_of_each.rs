// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Sui validators with over two thirds of the stake, and one anchovy
//! validator. Sui certifies checkpoints on its own, so anchovy's votes and
//! signatures can hide no discrepancy; anchovy certifies only the
//! checkpoints it built, and on a certificate of one it did not build (a
//! fork) it panics, aborting the test. So a test needs only wait for
//! anchovy to certify what it submitted.

#[path = "../../node/tests/genesis/mod.rs"]
mod genesis;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use consensus_glue::{Parameters, Runner};
use fastcrypto::traits::{KeyPair as _, ToFromBytes as _};
use messages::base::Digest;
use messages::base::ObjectId;
use move_core_types::ident_str;
use node::epoch::{Node, Perpetual, ValidatorKeys};
use protocol_config::Chain;
use rand::rngs::OsRng;
use sui_config::NodeConfig;
use sui_core::authority::AuthorityState;
use sui_core::authority_client::{AuthorityAPI as _, NetworkAuthorityClient};
use sui_swarm_config::genesis_config::ValidatorGenesisConfigBuilder;
use sui_swarm_config::network_config_builder::ConfigBuilder;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::committee::CommitteeTrait as _;
use sui_types::crypto::{AccountKeyPair, NetworkKeyPair};
use sui_types::digests::TransactionDigest;
use sui_types::effects::TransactionEffects;
use sui_types::effects::TransactionEffectsAPI as _;
use sui_types::gas_coin::{GAS, GasCoin};
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{CheckpointContents, CheckpointSummary};
use sui_types::messages_grpc::SubmitTxRequest;
use sui_types::object::{Object, Owner};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, CallArg, Command, FundsWithdrawalArg, ObjectArg, SharedObjectMutability,
};
use sui_types::transaction::{SenderSignedData, Transaction, TransactionData};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_FRAMEWORK_PACKAGE_ID, TypeTag};
use tokio::runtime::Runtime;
use validator::Validator;
use validator::current::CurrentEpoch;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::CheckpointStore;
use validator::proto::{
    RawSubmitTxRequest, RawValidatorSubmitStatus, RawWaitForEffectsRequest, SubmitTxType,
};
use validator::service::validator_server::Validator as _;
use workqueue::Inbox;

const TIMEOUT: Duration = Duration::from_mins(2);
const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;
/// A validator's stake in sui's test networks.
const STAKE: u64 = 20_000_000_000_000_000;

/// Anchovy's node, which the test drives itself to see what it builds.
struct Anchovy {
    /// None only while restarting.
    node: Option<Node>,
    runner: Runner,
    certified_inbox: Inbox<CertifiedCheckpoint>,
    certified: BTreeSet<u64>,
    // Dropped after the node.
    dir: tempfile::TempDir,
}

/// Not waiting at start for peers to return anchovy's last block: its
/// database survives its crashes.
fn parameters() -> Parameters {
    Parameters {
        sync_last_known_own_block_timeout: Duration::ZERO,
        ..Parameters::default()
    }
}

struct Network {
    runtime: Runtime,
    /// Validators with a quorum between them.
    sui: Vec<sui_swarm::memory::Node>,
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

/// What explains a fork, once anchovy has detected one: the first
/// checkpoint the two sides built differently, from their stores.
struct ForkReport {
    sui: Arc<AuthorityState>,
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
}

static FORK_REPORT: Mutex<Option<ForkReport>> = Mutex::new(None);

/// On anchovy's fork panic, before it aborts the process, prints the report.
fn report_forks() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info
                .payload_as_str()
                .is_some_and(|m| m.starts_with("fork:"))
                && let Some(report) = &*FORK_REPORT.lock().unwrap()
            {
                eprintln!("{}", report.report());
            }
            default(info);
        }));
    });
}

impl ForkReport {
    /// Both summaries of the first checkpoint built differently, the
    /// transactions only one side lists, and the effects of those both list
    /// with different effects.
    fn report(&self) -> String {
        let checkpoints = self.sui.get_checkpoint_store();
        for seq in 1.. {
            let (Some(theirs), Some(ours)) = (
                checkpoints.get_locally_computed_checkpoint(seq).unwrap(),
                self.checkpoints.built(seq),
            ) else {
                return format!("no checkpoint through {seq} built differently");
            };
            let summary: CheckpointSummary = bcs::from_bytes(&ours.summary).unwrap();
            if theirs.digest() == summary.digest() {
                continue;
            }
            let their_contents = checkpoints
                .get_checkpoint_contents(&theirs.content_digest)
                .unwrap()
                .expect("sui keeps the contents of what it built");
            let our_contents: CheckpointContents = bcs::from_bytes(&ours.contents).unwrap();
            let mut report =
                format!("checkpoint {seq} forked\nsui: {theirs:#?}\nanchovy: {summary:#?}\n");
            let diff = self.contents_diff(&their_contents, &our_contents);
            if diff.is_empty() {
                // The same transactions and effects: an order, or a field.
                write!(
                    report,
                    "sui: {their_contents:#?}\nanchovy: {our_contents:#?}"
                )
                .unwrap();
            }
            report += &diff;
            return report;
        }
        unreachable!()
    }

    fn contents_diff(&self, theirs: &CheckpointContents, ours: &CheckpointContents) -> String {
        let listed = |contents: &CheckpointContents| -> BTreeMap<TransactionDigest, _> {
            contents
                .iter()
                .map(|d| (d.transaction, d.effects))
                .collect()
        };
        let (their_listed, our_listed) = (listed(theirs), listed(ours));
        let cache = self.sui.get_transaction_cache_reader();
        let mut diff = String::new();
        for (digest, their_effects) in &their_listed {
            match our_listed.get(digest) {
                None => {
                    let transaction = cache
                        .get_transaction_block(digest)
                        .map(|t| t.data().transaction_data().clone());
                    writeln!(diff, "only sui lists {digest}: {transaction:#?}").unwrap();
                }
                Some(our_effects) if our_effects != their_effects => {
                    let theirs = cache.get_executed_effects(digest);
                    let ours = execution::executed(&self.store, &digest.into_inner())
                        .unwrap()
                        .map(|e| bcs::from_bytes::<TransactionEffects>(&e.effects).unwrap());
                    writeln!(
                        diff,
                        "{digest}: effects differ\nsui: {theirs:#?}\nanchovy: {ours:#?}"
                    )
                    .unwrap();
                }
                Some(_) => {}
            }
        }
        for digest in our_listed.keys().filter(|d| !their_listed.contains_key(d)) {
            let transaction = self
                .store
                .transaction(&Digest::new(digest.into_inner()))
                .unwrap()
                .map(|t| {
                    bcs::from_bytes::<SenderSignedData>(t.wire_bytes())
                        .unwrap()
                        .transaction_data()
                        .clone()
                });
            writeln!(diff, "only anchovy lists {digest}: {transaction:#?}").unwrap();
        }
        diff
    }
}

impl Network {
    /// A genesis of three validators, as sui's test networks make it: two
    /// run by sui, the last by anchovy. Voting power is capped at a third
    /// of it each (`voting_power::set_voting_power`), so the two with more
    /// stake have 3,334 votes each, a quorum together, and anchovy 3,332,
    /// too few to reject a transaction.
    fn new() -> Network {
        Network::with_epoch_duration(None)
    }

    /// As `new`, its epochs lasting `epoch_duration_ms`, if given.
    fn with_epoch_duration(epoch_duration_ms: Option<u64>) -> Network {
        telemetry_subscribers::init_for_testing();
        let validators = [3 * STAKE, 3 * STAKE, STAKE]
            .into_iter()
            .map(|stake| {
                ValidatorGenesisConfigBuilder::new()
                    .with_stake(stake)
                    .build(&mut OsRng)
            })
            .collect();
        let mut builder = ConfigBuilder::new_with_temp_dir().with_validators(validators);
        if let Some(duration) = epoch_duration_ms {
            builder = builder.with_epoch_duration(duration);
        }
        let config = builder.build();
        let (sui_configs, [anchovy_config]) = config.validator_configs.split_at(2) else {
            unreachable!("three validators");
        };
        let committee = config.genesis.committee();
        let votes = |c: &NodeConfig| committee.weight(&c.protocol_public_key());
        assert!(sui_configs.iter().map(votes).sum::<u64>() >= committee.quorum_threshold());
        assert!(votes(anchovy_config) < committee.validity_threshold());

        let runtime = Runtime::new().unwrap();
        let sui: Vec<_> = sui_configs
            .iter()
            .map(|c| {
                let node = sui_swarm::memory::Node::new(c.clone());
                runtime.block_on(node.start()).unwrap();
                node
            })
            .collect();
        let sui_config = &sui_configs[0];
        let sui_client = runtime
            .block_on(NetworkAuthorityClient::connect(
                &sui_config.network_address,
                sui_config.network_key_pair().public().clone(),
            ))
            .unwrap();
        let key = config.account_keys[0].copy();
        let sender = SuiAddress::from(key.public());
        report_forks();
        let (anchovy, coins) = Anchovy::start(anchovy_config, &config.genesis, sender);
        *FORK_REPORT.lock().unwrap() = Some(ForkReport {
            sui: sui[0].get_node_handle().unwrap().state(),
            store: anchovy.node().perpetual.store.clone(),
            checkpoints: anchovy.checkpoints(),
        });
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
            .node()
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

    /// Sends `amount` split off coin `i` to the sender's address balance.
    fn send_funds(&self, i: usize, amount: u64) -> Transaction {
        let sender = self.sender;
        self.ptb(i, |b| {
            let amount = b.pure(amount).unwrap();
            let coin = b.command(Command::SplitCoins(Argument::GasCoin, vec![amount]));
            let recipient = b.pure(sender).unwrap();
            b.programmable_move_call(
                SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("coin").to_owned(),
                ident_str!("send_funds").to_owned(),
                vec![GAS::type_tag()],
                vec![coin, recipient],
            );
        })
    }

    /// Withdraws `amount` from the sender's address balance into a coin,
    /// paid with coin `i`.
    fn withdraw(&self, i: usize, amount: u64) -> Transaction {
        let sender = self.sender;
        self.ptb(i, |b| {
            let withdrawal = b
                .funds_withdrawal(FundsWithdrawalArg::balance_from_sender(
                    amount,
                    GAS::type_tag(),
                ))
                .unwrap();
            let coin = b.programmable_move_call(
                SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("coin").to_owned(),
                ident_str!("redeem_funds").to_owned(),
                vec![GAS::type_tag()],
                vec![withdrawal],
            );
            b.transfer_arg(sender, coin);
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
        let service = Validator::over(self.anchovy.node().perpetual.current.clone());
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
        let service = Validator::over(self.anchovy.node().perpetual.current.clone());
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

    /// Submits `transactions` through a sui validator's service, then waits as
    /// `execute_together`.
    fn execute_through_sui(&mut self, transactions: &[&Transaction]) -> Vec<TransactionEffects> {
        self.submit_through_sui(transactions);
        self.agree_on(transactions)
    }

    /// Submits `transactions` through a sui validator's service, one request
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

    /// Waits until anchovy has certified the checkpoints of `transactions`,
    /// each of which succeeded; their effects.
    fn agree_on(&mut self, transactions: &[&Transaction]) -> Vec<TransactionEffects> {
        transactions
            .iter()
            .map(|transaction| {
                let seq = self.wait_for("the transaction's checkpoint", |network| {
                    network.anchovy.checkpoint_of(transaction.digest())
                });
                self.wait_for_certified(seq);
                let effects = self
                    .anchovy
                    .effects(transaction.digest())
                    .expect("executed");
                assert!(effects.status().is_ok(), "{:?}", effects.status());
                effects
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
                "timed out waiting for {what}: sui certified through {:?}, anchovy {:?}",
                self.sui()
                    .get_checkpoint_store()
                    .get_highest_verified_checkpoint()
                    .unwrap()
                    .map(|c| *c.sequence_number()),
                self.anchovy.certified.last(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The first sui validator's state.
    fn sui(&self) -> Arc<AuthorityState> {
        self.sui[0]
            .get_node_handle()
            .expect("the sui node is running")
            .state()
    }

    /// Waits until anchovy has certified checkpoint `seq`, as sui did.
    fn wait_for_certified(&mut self, seq: u64) {
        self.wait_for(&format!("checkpoint {seq} to certify"), |network| {
            network.anchovy.certified.contains(&seq).then_some(())
        });
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
        let mut runner = Runner::new(&keys, parameters());
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
            node: Some(node),
            runner,
            certified_inbox,
            certified: BTreeSet::new(),
            dir,
        };
        (anchovy, genesis.coins)
    }

    /// Takes what anchovy certified, and moves it into the next epoch once
    /// its epoch has ended, as `Node::run`.
    fn poll(&mut self) {
        while let Some(certified) = self.certified_inbox.try_pop() {
            self.certified.insert(certified.seq);
        }
        let ended = self
            .node()
            .running()
            .is_some_and(|epoch| epoch.pipeline.ended.borrow().is_some());
        if ended {
            node::epoch::Consensus::stop(&mut self.runner);
            let node = self.node.as_mut().unwrap();
            node.finish_epoch();
            node.start_epoch(&mut self.runner);
            if let Some(report) = &mut *FORK_REPORT.lock().unwrap() {
                report.checkpoints = self.checkpoints();
            }
        }
    }

    fn node(&self) -> &Node {
        self.node.as_ref().expect("anchovy is running")
    }

    /// Stops anchovy wherever it is, as a crash would, and starts it again
    /// over its store as a new process: its pipeline resumes from what it
    /// committed, and consensus replays from its database.
    fn restart(&mut self) {
        // The report holds the store: let go, so that it closes.
        let sui = FORK_REPORT.lock().unwrap().take().map(|report| report.sui);
        node::epoch::Consensus::stop(&mut self.runner);
        let Perpetual {
            store,
            epochs,
            state_sync,
            chain,
            chain_identifier,
            keys,
            ..
        } = self.node.take().unwrap().into_perpetual();
        assert_eq!(Arc::strong_count(&store), 1, "only the node holds the store");
        drop(store);
        let store = Arc::new(store::Store::open(&self.dir.path().join("perpetual")).unwrap());
        self.runner = Runner::new(&keys, parameters());
        let mut node = Node::new(Perpetual {
            store: store.clone(),
            epochs,
            current: CurrentEpoch::new(),
            state_sync,
            chain,
            chain_identifier,
            keys,
        });
        node.start_epoch(&mut self.runner);
        self.node = Some(node);
        *FORK_REPORT.lock().unwrap() = sui.map(|sui| ForkReport {
            sui,
            store,
            checkpoints: self.checkpoints(),
        });
    }

    fn checkpoints(&self) -> Arc<CheckpointStore> {
        self.node().running().unwrap().pipeline.checkpoints.clone()
    }

    fn epoch(&self) -> u64 {
        self.node().running().unwrap().state.params.epoch
    }

    /// The checkpoint anchovy built listing `transaction`.
    fn checkpoint_of(&self, transaction: &TransactionDigest) -> Option<u64> {
        let checkpoints = &self.node().running()?.pipeline.checkpoints;
        (1..)
            .map_while(|seq| {
                let contents: CheckpointContents =
                    bcs::from_bytes(&checkpoints.built(seq)?.contents).unwrap();
                Some((seq, contents))
            })
            .find(|(_, contents)| contents.iter().any(|d| d.transaction == *transaction))
            .map(|(seq, _)| seq)
    }

    fn effects(&self, digest: &TransactionDigest) -> Option<TransactionEffects> {
        let executed =
            execution::executed(&self.node().perpetual.store, &digest.into_inner()).unwrap()?;
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
fn address_balances_settle_alike() {
    let mut network = Network::new();
    let send = network.send_funds(0, 3 * SUI);
    network.execute(&send);
    let withdraw = network.withdraw(1, SUI);
    network.execute(&withdraw);
    let through_sui = network.withdraw(2, SUI);
    network.execute_through_sui(&[&through_sui]);
    let again = network.send_funds(0, SUI);
    network.execute(&again);
    drop(network.runtime);
}

#[test]
fn epochs_follow_one_another_alike() {
    let mut network = Network::with_epoch_duration(Some(5_000));
    let first = network.transfer(0);
    network.execute(&first);
    network.wait_for("epoch 2", |network| {
        (network.anchovy.epoch() >= 2).then_some(())
    });
    let later = network.transfer(0);
    network.execute(&later);
    drop(network.runtime);
}

#[test]
fn anchovy_resumes_alike_after_a_crash() {
    let mut network = Network::with_epoch_duration(Some(10_000));
    let first = network.transfer(0);
    network.execute(&first);
    network.anchovy.restart();
    // Spending the first transfer's output.
    let again = network.transfer(0);
    network.execute(&again);
    // And into the next epoch, restarted again on the way.
    network.anchovy.restart();
    network.wait_for("epoch 1", |network| {
        (network.anchovy.epoch() >= 1).then_some(())
    });
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
