// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The whole pipeline, started by `Pipeline::start`, with the test playing
//! consensus for a committee of one validator: it votes on each block,
//! commits it, and puts the checkpoint signatures the pipeline submits into
//! the next commit, until the checkpoints certify.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use checkpoint::ProtocolKey;
use consensus::{
    Block, BlockRef, BlockStatus, CommittedSubDag, TransactionClient, TransactionsGuard,
};
use messages::base::ObjectId;
use move_core_types::ident_str;
mod genesis;

use fastcrypto::hash::MultisetHash;
use node::Pipeline;
use node::epoch::{Epoch, EpochRecord, Node, Perpetual};
use protocol_config::Chain;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, AuthorityPublicKeyBytes, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::execution_status::{ExecutionErrorKind, ExecutionStatus};
use sui_types::gas_coin::{GAS, GasCoin};
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CertifiedCheckpointSummary, CheckpointCommitment, CheckpointContents, CheckpointSummary,
};
use sui_types::object::{Object, Owner};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::supported_protocol_versions::ProtocolVersion;
use sui_types::transaction::TransactionDataAPI as _;
use sui_types::transaction::{
    Argument, CallArg, Command as PtbCommand, FundsWithdrawalArg, ObjectArg,
    SharedObjectMutability, Transaction, TransactionData,
};
use tokio::sync::{mpsc, oneshot};
use validator::Validator;
use validator::consensus::decode::CommitRequest;
use validator::consensus::status::{ConsensusPosition, RETENTION_ROUNDS};
use validator::consensus::vote::VoteRequest;
use validator::current::CurrentEpoch;
use validator::epoch::EpochState;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::BuiltCheckpoint;
use validator::proto::{
    RawSubmitTxRequest, RawValidatorSubmitStatus, RawValidatorTransactionStatus,
    RawWaitForEffectsRequest, SubmitTxType,
};
use validator::service::validator_server::Validator as _;
use workqueue::{Inbox, Queue};

const RGP: u64 = 1000;
const CHAIN: Chain = Chain::Unknown;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;
/// Long enough for a debug build; a hang fails instead of blocking.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Commit timestamps this far apart cut a checkpoint per commit.
const COMMIT_INTERVAL_MS: u64 = 10_000;

/// Panics on any thread so far. Workers catch an item's panic and carry on,
/// so a passing assertion could otherwise hide one.
static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current().name().unwrap_or("?").to_owned();
            PANICS.lock().unwrap().push(format!("{thread}: {info}"));
            default(info);
        }));
    });
}

fn assert_no_panics() {
    // Cloned, as the hook takes the lock when this assertion panics.
    let panics = PANICS.lock().unwrap().clone();
    assert!(panics.is_empty(), "panics: {panics:#?}");
}

/// Waits until `f` gives something, polling.
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert_no_panics();
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A one-validator network: the node, state sync's end of its queue, and
/// the consensus the test plays, one epoch at a time.
struct Network {
    /// None while crashed.
    node: Option<Node>,
    /// State sync's inbox, kept across crashes: what the aggregator certified.
    state_sync: (Queue<CertifiedCheckpoint>, Inbox<CertifiedCheckpoint>),
    sender: SuiAddress,
    key: AccountKeyPair,
    sui_committee: sui_types::committee::Committee,
    /// The current epoch's last commit, from 1 each epoch.
    commit_index: u32,
    commit_interval_ms: u64,
    /// The last commit's timestamp, across epochs.
    clock_ms: u64,
    /// The checkpoint the current epoch's first follows.
    checkpoint_offset: u64,
    /// The consensus client's lanes, normal and priority.
    lanes: (
        mpsc::Receiver<TransactionsGuard>,
        mpsc::Receiver<TransactionsGuard>,
    ),
    client: TransactionClient,
    /// Submissions taken and not yet in a block: each goes in the next.
    pending: Vec<TransactionsGuard>,
    /// What became of each block with submissions, once committed.
    blocks: Vec<(BlockRef, oneshot::Sender<BlockStatus>)>,
    certified: BTreeMap<u64, CertifiedCheckpointSummary>,
    /// Every commit of the current epoch delivered, which consensus replays
    /// to a restarted pipeline.
    delivered: Vec<CommittedSubDag>,
    /// The sender's gas coins, by id.
    coins: Vec<ObjectID>,
    /// Every object genesis made, by id.
    genesis_objects: Vec<ObjectID>,
    // Dropped after the pipeline.
    dir: tempfile::TempDir,
}

impl Network {
    /// A genesis giving the sender `coins` gas coins of 10 SUI, with an
    /// epoch longer than any test.
    fn new(coins: usize) -> Network {
        Network::with_epoch_duration(coins, 1 << 40)
    }

    /// A genesis giving the sender `coins` gas coins of 10 SUI, its first
    /// epoch lasting `epoch_duration_ms`.
    fn with_epoch_duration(coins: usize, epoch_duration_ms: u64) -> Network {
        record_panics();
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(dir.path()).unwrap());
        let genesis = genesis::build(sender, coins, 10 * SUI, epoch_duration_ms);
        execution::genesis::commit(&store, &genesis.objects).unwrap();
        node::epoch::genesis(
            &store,
            &genesis.checkpoint,
            genesis.objects.iter().map(|o| o.digest().into_inner()),
        );
        let protocol_key = Arc::new(ProtocolKey::new(genesis::protocol_key_pair()));
        let chain_identifier = messages::base::Digest::new(genesis.chain_identifier);

        let sui_committee = sui_types::committee::Committee::new(
            0,
            BTreeMap::from([(AuthorityPublicKeyBytes::new(*protocol_key.public()), 10_000)]),
        );
        let state_sync = workqueue::queue(1024);
        let (client, normal, priority) = TransactionClient::new(64);
        let lanes = (normal, priority);
        let perpetual = Perpetual {
            store,
            current: CurrentEpoch::new(),
            state_sync: state_sync.0.clone(),
            chain: CHAIN,
            chain_identifier,
            protocol_key,
        };
        let mut node = Node::new(perpetual);
        node.start_epoch(|_, _| client.clone());
        Network {
            node: Some(node),
            state_sync,
            sender,
            key,
            sui_committee,
            commit_index: 0,
            commit_interval_ms: COMMIT_INTERVAL_MS,
            clock_ms: 0,
            checkpoint_offset: 0,
            lanes,
            client,
            pending: Vec::new(),
            blocks: Vec::new(),
            certified: BTreeMap::new(),
            delivered: Vec::new(),
            coins: genesis.coins,
            genesis_objects: genesis.objects.iter().map(|o| o.id()).collect(),
            dir,
        }
    }

    fn node(&self) -> &Node {
        self.node.as_ref().expect("the node is running")
    }

    fn running(&self) -> &Epoch {
        self.node().running().expect("an epoch is running")
    }

    fn pipeline(&self) -> &Pipeline {
        &self.running().pipeline
    }

    fn epoch(&self) -> &Arc<EpochState> {
        &self.running().state
    }

    fn store(&self) -> &Arc<store::Store> {
        &self.node().perpetual.store
    }

    /// The validator service, over whichever epoch runs.
    fn service(&self) -> Validator {
        Validator::over(self.node().perpetual.current.clone())
    }

    /// Consensus for a new epoch: a new client, and commits from 1.
    fn new_consensus(&mut self) {
        let (client, normal, priority) = TransactionClient::new(64);
        self.client = client;
        self.lanes = (normal, priority);
        self.pending.clear();
        self.blocks.clear();
        self.delivered.clear();
        self.commit_index = 0;
    }

    /// Starts the node's current epoch over the current consensus.
    fn start_epoch(&mut self) {
        let client = self.client.clone();
        let node = self.node.as_mut().expect("the node is running");
        let epoch = node.start_epoch(|_, _| client).state.params.epoch;
        let record = EpochRecord::load(&node.perpetual.store);
        assert_eq!(record.epoch, epoch);
        self.checkpoint_offset = record.previous_checkpoint;
    }

    /// Plays consensus until the epoch ends, then moves the node into the
    /// next epoch, with consensus for it. The ended epoch's last checkpoint.
    fn next_epoch(&mut self) -> u64 {
        let last = self.end_epoch();
        // The ended epoch's consensus stops first.
        self.new_consensus();
        self.node.as_mut().unwrap().finish_epoch();
        self.start_epoch();
        last
    }

    /// Stops the node and reopens its store, so whatever it had not
    /// committed is gone.
    fn crash(&mut self) {
        // The epoch's workers hold the store: stopped first, it closes.
        let Perpetual {
            store,
            current,
            protocol_key,
            chain_identifier,
            ..
        } = self.node.take().unwrap().into_perpetual();
        drop(current);
        assert_eq!(
            Arc::strong_count(&store),
            1,
            "only the node holds the store"
        );
        drop(store);
        let store = Arc::new(store::Store::open(self.dir.path()).unwrap());
        // Checkpoints certified and not committed are certified again.
        let committed = store.committed_checkpoint().unwrap().unwrap();
        self.certified.retain(|seq, _| *seq <= committed);
        self.node = Some(Node::new(Perpetual {
            store,
            current: CurrentEpoch::new(),
            state_sync: self.state_sync.0.clone(),
            chain: CHAIN,
            chain_identifier,
            protocol_key,
        }));
    }

    /// Stops the running epoch, leaving the node between epochs.
    fn stop_epoch(&mut self) {
        self.node.as_mut().unwrap().stop();
    }

    /// Starts the node's epoch again, and replays the epoch's commits to
    /// it, as consensus does from its own store. If the epoch had ended
    /// before the crash, the node starts the next, with consensus for it.
    fn recover(&mut self) {
        if node::epoch::epoch_ended(self.store()) {
            self.new_consensus();
        }
        self.start_epoch();
        let running = self.running();
        for commit in &self.delivered {
            assert!(
                running
                    .pipeline
                    .commits
                    .try_push(CommitRequest {
                        epoch: running.state.params.clone(),
                        commit: commit.clone(),
                    })
                    .is_ok()
            );
        }
    }

    fn live(&self, id: ObjectID) -> Object {
        let object = self
            .store()
            .live_object(&ObjectId(id.into_bytes()))
            .unwrap()
            .unwrap();
        bcs::from_bytes(object.wire_bytes()).unwrap()
    }

    /// Gas coin `i`, as of now.
    fn coin(&self, i: u64) -> ObjectRef {
        let id = self.coins[usize::try_from(i).unwrap()];
        self.live(id).compute_object_reference()
    }

    fn sign(&self, data: TransactionData) -> Transaction {
        Transaction::from_data_and_signer(data, vec![&self.key])
    }

    fn ptb(
        &self,
        gas: ObjectRef,
        f: impl FnOnce(&mut ProgrammableTransactionBuilder),
    ) -> Transaction {
        let mut builder = ProgrammableTransactionBuilder::new();
        f(&mut builder);
        self.sign(TransactionData::new_programmable(
            self.sender,
            vec![gas],
            builder.finish(),
            BUDGET,
            RGP,
        ))
    }

    /// 1 SUI to a fresh address, paid with `gas`.
    fn transfer(&self, gas: ObjectRef) -> Transaction {
        self.sign(TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            gas,
            BUDGET,
            RGP,
        ))
    }

    /// Splits 1 SUI off `gas` and shares the new coin.
    fn share(&self, gas: ObjectRef) -> Transaction {
        self.ptb(gas, |b| {
            let amount = b.pure(SUI).unwrap();
            let coin = b.command(PtbCommand::SplitCoins(Argument::GasCoin, vec![amount]));
            b.programmable_move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("transfer").to_owned(),
                ident_str!("public_share_object").to_owned(),
                vec![sui_types::TypeTag::Struct(Box::new(GasCoin::type_()))],
                vec![coin],
            );
        })
    }

    /// Writes the shared coin `id`: splits a MIST off it, to the sender.
    fn split_shared(&self, gas: ObjectRef, shared: ObjectArg) -> Transaction {
        self.ptb(gas, |b| {
            b.move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
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

    /// Sends `amount` split off `gas` to the sender's address balance.
    fn send_funds(&self, gas: ObjectRef, amount: u64) -> Transaction {
        let sender = self.sender;
        self.ptb(gas, |b| {
            let amount = b.pure(amount).unwrap();
            let coin = b.command(PtbCommand::SplitCoins(Argument::GasCoin, vec![amount]));
            let recipient = b.pure(sender).unwrap();
            b.programmable_move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("coin").to_owned(),
                ident_str!("send_funds").to_owned(),
                vec![GAS::type_tag()],
                vec![coin, recipient],
            );
        })
    }

    /// Withdraws `amount` from the sender's address balance into a coin.
    fn withdraw(&self, gas: ObjectRef, amount: u64) -> Transaction {
        let sender = self.sender;
        self.ptb(gas, |b| {
            let withdrawal = b
                .funds_withdrawal(FundsWithdrawalArg::balance_from_sender(
                    amount,
                    GAS::type_tag(),
                ))
                .unwrap();
            let coin = b.programmable_move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("coin").to_owned(),
                ident_str!("redeem_funds").to_owned(),
                vec![GAS::type_tag()],
                vec![withdrawal],
            );
            b.transfer_arg(sender, coin);
        })
    }

    /// Reads the clock.
    fn read_clock(&self, gas: ObjectRef) -> Transaction {
        let clock = shared_arg(&self.live(sui_types::SUI_CLOCK_OBJECT_ID), false);
        self.ptb(gas, |b| {
            b.move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("clock").to_owned(),
                ident_str!("timestamp_ms").to_owned(),
                vec![],
                vec![CallArg::Object(clock)],
            )
            .unwrap();
        })
    }

    /// Takes what the pipeline handed consensus and state sync.
    fn poll(&mut self) {
        while let Ok(guard) = self.lanes.1.try_recv() {
            self.pending.push(guard);
        }
        while let Ok(guard) = self.lanes.0.try_recv() {
            self.pending.push(guard);
        }
        while let Some(certified) = self.state_sync.1.try_pop() {
            let summary: CertifiedCheckpointSummary = bcs::from_bytes(&certified.summary).unwrap();
            assert_eq!(summary.sequence_number, certified.seq);
            // The validator set never changes: each epoch's committee is the
            // same.
            let committee = sui_types::committee::Committee::new(
                summary.epoch,
                self.sui_committee.voting_rights.iter().copied().collect(),
            );
            summary.verify_authority_signatures(&committee).unwrap();
            assert!(
                self.certified.insert(certified.seq, summary).is_none(),
                "checkpoint {} certified twice",
                certified.seq
            );
        }
    }

    /// Votes on a block of `transactions` (each `ConsensusTransaction`
    /// BCS) and whatever was submitted to consensus (checkpoint signatures,
    /// transactions through the RPC service), then commits it. The commit's
    /// checkpoint sequence number.
    fn commit(&mut self, transactions: Vec<Vec<u8>>) -> u64 {
        self.commit_outvoted(transactions, &[])
    }

    /// As `commit`, where this validator votes to reject the transactions
    /// at `rejects` and the rest of the committee accepts them.
    fn commit_outvoted(&mut self, transactions: Vec<Vec<u8>>, rejects: &[u16]) -> u64 {
        self.poll();
        self.commit_index += 1;
        self.clock_ms += self.commit_interval_ms;
        let index = self.commit_index;
        let reference = BlockRef {
            round: index,
            author: 0,
            digest: [index as u8; 32],
        };
        // Each submission is included in this block, where it is told.
        let mut transactions = transactions;
        for guard in std::mem::take(&mut self.pending) {
            let first = transactions.len() as u16;
            let indices = (first..first + guard.transactions.len() as u16).collect();
            transactions.extend(guard.transactions);
            let (status, receiver) = oneshot::channel();
            let _ = guard
                .included_in_block_ack
                .send((reference, indices, receiver));
            self.blocks.push((reference, status));
        }
        let block = Block {
            reference,
            transactions,
        };

        let commit = CommittedSubDag {
            commit_index: index,
            digest: [index as u8; 32],
            leader: reference,
            timestamp_ms: self.clock_ms,
            blocks: vec![block.clone()],
            rejected: vec![],
        };
        self.delivered.push(commit.clone());
        let (reply, mut verdict) = tokio::sync::oneshot::channel();
        assert!(
            self.pipeline()
                .votes
                .try_push(VoteRequest {
                    epoch: self.epoch().clone(),
                    block: block.clone(),
                    reply,
                })
                .is_ok()
        );
        let verdict = wait_for("the vote", || verdict.try_recv().ok());
        assert_eq!(verdict, Ok(rejects.to_vec()), "the vote on commit {index}");

        assert!(
            self.pipeline()
                .commits
                .try_push(CommitRequest {
                    epoch: self.epoch().params.clone(),
                    commit: commit.clone(),
                })
                .is_ok()
        );
        for (block, status) in self.blocks.drain(..) {
            let _ = status.send(BlockStatus::Sequenced(block));
        }
        // One checkpoint per commit, numbered on from the previous epoch's.
        self.checkpoint_offset + u64::from(index)
    }

    /// Plays consensus until the epoch's last checkpoint has committed: its
    /// sequence number.
    fn end_epoch(&mut self) -> u64 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.poll();
            if let Some(seq) = *self.pipeline().ended.borrow() {
                return seq;
            }
            assert_no_panics();
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the epoch to end"
            );
            // Commits carry the checkpoint signatures, and move the clock to
            // the epoch's end.
            self.commit(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Plays consensus until checkpoint `seq` certifies: commits each
    /// checkpoint signature the pipeline submits.
    fn certify(&mut self, seq: u64) -> CertifiedCheckpointSummary {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.poll();
            if let Some(summary) = self.certified.get(&seq) {
                return summary.clone();
            }
            if self.pending.is_empty() {
                assert_no_panics();
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for checkpoint {seq} to certify; certified: {:?}, \
                     settled chunks: {:?}, built: {:?}",
                    self.certified.keys().collect::<Vec<_>>(),
                    (1..=u64::from(self.commit_index))
                        .filter(|h| self.pipeline().chunks.get(*h).is_some())
                        .collect::<Vec<_>>(),
                    (0..=seq + 2)
                        .filter(|s| self.pipeline().checkpoints.is_built(*s))
                        .collect::<Vec<_>>(),
                );
                std::thread::sleep(Duration::from_millis(2));
            } else {
                self.commit(vec![]);
            }
        }
    }

    /// The live object set's ECMH: every object genesis or a checkpoint
    /// through `last` wrote, at its live version. Recomputed from the store,
    /// apart from the state hasher.
    fn live_set(&self, last: u64) -> state_hasher::StateHash {
        let mut ids: BTreeSet<ObjectID> = self.genesis_objects.iter().copied().collect();
        for seq in 1..=last {
            for digest in self.contents(seq) {
                for (object, _, _) in self.effects(&digest).all_changed_objects() {
                    ids.insert(object.0);
                }
            }
        }
        let mut live = state_hasher::StateHash::default();
        for id in ids {
            if let Some(object) = self.store().live(&ObjectId(id.into_bytes())).unwrap() {
                live.insert(object.digest.bytes);
            }
        }
        live
    }

    /// The effects digest of `transaction`, if executed.
    fn executed(&self, transaction: &Transaction) -> Option<messages::base::Digest> {
        let digest = messages::base::Digest::new(transaction.digest().into_inner());
        self.store().executed_effects(&digest).unwrap()
    }

    fn wait_executed(&self, transaction: &Transaction) -> messages::base::Digest {
        wait_for(&format!("{} to execute", transaction.digest()), || {
            self.executed(transaction)
        })
    }

    fn effects(&self, digest: &TransactionDigest) -> TransactionEffects {
        let executed = execution::executed(self.store(), &digest.into_inner())
            .unwrap()
            .unwrap_or_else(|| panic!("{digest} executed"));
        bcs::from_bytes(&executed.effects).unwrap()
    }

    /// Checkpoint `seq` once committed: certified checkpoints commit in
    /// the background.
    fn committed(&self, seq: u64) -> BuiltCheckpoint {
        let bytes = wait_for(&format!("checkpoint {seq} to commit"), || {
            self.store().checkpoint(seq).unwrap()
        });
        BuiltCheckpoint::decode(&bytes)
    }

    /// The transactions committed checkpoint `seq` lists.
    fn contents(&self, seq: u64) -> Vec<TransactionDigest> {
        let built = self.committed(seq);
        let contents: CheckpointContents = bcs::from_bytes(&built.contents).unwrap();
        contents.iter().map(|d| d.transaction).collect()
    }

    /// Asserts each of `transactions` executed successfully and is listed
    /// in checkpoint `seq`, with the effects the store holds.
    fn assert_checkpointed(&self, seq: u64, transactions: &[&Transaction]) {
        let built = self.committed(seq);
        let contents: CheckpointContents = bcs::from_bytes(&built.contents).unwrap();
        let listed: Vec<_> = contents.iter().collect();
        for transaction in transactions {
            let digest = *transaction.digest();
            let effects = self.effects(&digest);
            assert_eq!(effects.status(), &ExecutionStatus::Success, "{digest}");
            let entry = listed
                .iter()
                .find(|d| d.transaction == digest)
                .unwrap_or_else(|| panic!("checkpoint {seq} lists {digest}"));
            assert_eq!(entry.effects, effects.digest());
        }
    }
}

fn shared_arg(object: &Object, mutable: bool) -> ObjectArg {
    let Owner::Shared {
        initial_shared_version,
    } = object.owner
    else {
        panic!("{} is shared", object.id());
    };
    ObjectArg::SharedObject {
        id: object.id(),
        initial_shared_version,
        mutability: if mutable {
            SharedObjectMutability::Mutable
        } else {
            SharedObjectMutability::Immutable
        },
    }
}

/// A `UserTransactionV2` of `transaction`, claiming its one signature and no
/// alias, as `SubmitChecker` builds it.
fn consensus_transaction(transaction: &Transaction) -> Vec<u8> {
    let mut bytes = transaction.digest().inner()[..8].to_vec();
    bytes.push(12);
    bytes.extend_from_slice(&bcs::to_bytes(transaction).unwrap());
    bytes.extend_from_slice(&[1, 2, 1, 0, 0]);
    bytes
}

#[test]
fn committed_transactions_execute_and_their_checkpoints_certify() {
    let mut network = Network::new(3);

    // Round 1: owned transactions, one sharing a coin.
    let transfer = network.transfer(network.coin(0));
    let share = network.share(network.coin(1));
    let other = network.transfer(network.coin(2));
    let round_1 = [&transfer, &share, &other];
    let seq_1 = network.commit(round_1.iter().map(|t| consensus_transaction(t)).collect());
    let first = network.certify(seq_1);
    network.assert_checkpointed(seq_1, &round_1);
    assert_eq!(first.sequence_number, seq_1);

    // Round 2: a transfer of round 1's output, a write of the coin it
    // shared, and a read of the clock.
    let created = network.effects(share.digest()).created();
    let (shared_ref, _) = created
        .iter()
        .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
        .expect("a shared coin");
    let shared = shared_arg(&network.live(shared_ref.0), true);
    let again = network.transfer(network.coin(0));
    let write = network.split_shared(network.coin(1), shared);
    let clock = network.read_clock(network.coin(2));
    let round_2 = [&again, &write, &clock];
    let seq_2 = network.commit(round_2.iter().map(|t| consensus_transaction(t)).collect());
    assert!(seq_2 > seq_1);
    let second = network.certify(seq_2);
    network.assert_checkpointed(seq_2, &round_2);

    // Each checkpoint chains to the one before.
    for seq in seq_1 + 1..=seq_2 {
        let previous = network.certify(seq - 1);
        let summary = network.certify(seq);
        assert_eq!(summary.previous_digest, Some(*previous.digest()), "{seq}");
        assert_eq!(summary.epoch, 0);
    }
    assert_eq!(
        second.previous_digest,
        Some(*network.certify(seq_2 - 1).digest())
    );
    assert!(second.network_total_transactions > first.network_total_transactions);

    // No transaction is listed twice.
    let mut seen = HashSet::new();
    for seq in 1..=seq_2 {
        for digest in network.contents(seq) {
            assert!(seen.insert(digest), "{digest} listed twice");
        }
    }
    assert_no_panics();
}

/// Crashes before anything but genesis commits, and again with one
/// checkpoint committed and later work executed: each time the work not
/// committed is lost, replay executes it again, identically, and checkpoints
/// carry on from the committed one.
#[test]
fn a_crash_loses_only_uncommitted_work_which_replay_redoes() {
    let mut network = Network::new(3);
    let first = network.transfer(network.coin(0));
    let seq_1 = network.commit(vec![consensus_transaction(&first)]);
    let executed = network.wait_executed(&first);

    network.crash();
    assert_eq!(network.store().committed_checkpoint().unwrap(), Some(0));
    assert!(network.executed(&first).is_none());
    network.recover();
    assert_eq!(network.wait_executed(&first), executed);
    network.certify(seq_1);
    network.assert_checkpointed(seq_1, &[&first]);

    let second = network.transfer(network.coin(1));
    let seq_2 = network.commit(vec![consensus_transaction(&second)]);
    let executed = network.wait_executed(&second);
    network.crash();
    let committed = network.store().committed_checkpoint().unwrap().unwrap();
    assert!((seq_1..seq_2).contains(&committed));
    assert!(network.executed(&first).is_some());
    assert!(network.executed(&second).is_none());
    network.recover();
    assert_eq!(network.wait_executed(&second), executed);

    let third = network.transfer(network.coin(2));
    let seq_3 = network.commit(vec![consensus_transaction(&third)]);
    network.certify(seq_3);
    network.assert_checkpointed(seq_2, &[&second]);
    network.assert_checkpointed(seq_3, &[&third]);
    // Rebuilt checkpoints are the ones committed before: each summary
    // follows the stored one before it.
    for seq in 1..=seq_3 {
        let previous = network.committed(seq - 1);
        let summary: CheckpointSummary = bcs::from_bytes(&network.committed(seq).summary).unwrap();
        assert_eq!(
            summary.previous_digest.unwrap().into_inner(),
            previous.digest
        );
    }
    assert_no_panics();
}

/// A shared object first touched by the transaction deleting it: after a
/// crash, replay finds that transaction executed and the object gone, and
/// takes the version it read from its effects.
#[test]
fn replay_reads_a_since_deleted_shared_object_from_effects() {
    let mut network = Network::new(3);
    let share = network.share(network.coin(0));
    let seq = network.commit(vec![consensus_transaction(&share)]);
    network.certify(seq);
    let created = network.effects(share.digest()).created();
    let (shared_ref, _) = created
        .iter()
        .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
        .expect("a shared coin");
    let shared = shared_arg(&network.live(shared_ref.0), true);
    let delete = network.ptb(network.coin(1), |b| {
        let coin = b.obj(shared).unwrap();
        b.command(PtbCommand::MergeCoins(Argument::GasCoin, vec![coin]));
    });
    let seq = network.commit(vec![consensus_transaction(&delete)]);
    network.certify(seq);
    network.assert_checkpointed(seq, &[&delete]);
    wait_for("the deletion to commit", || {
        (network.store().committed_checkpoint().unwrap() >= Some(seq)).then_some(())
    });

    network.crash();
    network.recover();
    let after = network.transfer(network.coin(2));
    let seq = network.commit(vec![consensus_transaction(&after)]);
    network.certify(seq);
    network.assert_checkpointed(seq, &[&after]);
    assert_no_panics();
}

/// The first commit at or past the epoch's end is its last: its last
/// checkpoint ends with the change-epoch transaction, and carries the next
/// epoch's committee and protocol version and the epoch's root. Once it
/// commits, the system state is the next epoch's, and the epoch is over.
#[test]
fn the_epoch_ends_with_a_checkpoint_closing_it() {
    // Commits are 10 s apart from 10 s: the third is the epoch's last.
    let mut network = Network::with_epoch_duration(2, 25_000);
    let transfer = network.transfer(network.coin(0));
    network.commit(vec![consensus_transaction(&transfer)]);
    network.commit(vec![]);
    let last_commit = network.commit(vec![]);
    let last = network.end_epoch();
    assert!(
        last >= last_commit,
        "the last commit's checkpoint ends the epoch"
    );
    network.assert_checkpointed(1, &[&transfer]);

    // Its summary closes the epoch, as certified.
    let certified = network.certify(last);
    let end = certified
        .end_of_epoch_data
        .as_ref()
        .expect("the last checkpoint has end-of-epoch data");
    assert_eq!(end.next_epoch_protocol_version, ProtocolVersion::MAX);
    assert_eq!(
        end.next_epoch_committee,
        network.sui_committee.voting_rights
    );
    let committed = network.committed(last);
    let root = state_hasher::decode(committed.root_state.as_ref().unwrap());
    assert_eq!(
        end.epoch_commitments,
        vec![CheckpointCommitment::ECMHLiveObjectSetDigest(
            root.digest().into()
        )]
    );
    for seq in 1..last {
        assert!(network.certify(seq).end_of_epoch_data.is_none(), "{seq}");
    }
    assert_eq!(
        network.live_set(last).digest(),
        root.digest(),
        "the root is the live object set's"
    );
    // It ends with the change-epoch transaction, committed with it.
    let contents = network.contents(last);
    let change_epoch = contents.last().unwrap();
    let transaction = network
        .store()
        .transaction(&messages::base::Digest::new(change_epoch.into_inner()))
        .unwrap()
        .unwrap();
    let transaction: sui_types::transaction::SenderSignedData =
        bcs::from_bytes(transaction.wire_bytes()).unwrap();
    assert!(transaction.transaction_data().is_end_of_epoch_tx());
    let state = execution::system_state::read(network.store()).unwrap();
    assert_eq!(state.epoch, 1);
    assert_eq!(state.epoch_start_timestamp_ms, committed.timestamp_ms);
    assert_no_panics();
}

/// Epochs follow one another: each is built from the system state the
/// previous one's last checkpoint committed, its checkpoints continue the
/// previous one's, and transactions keep executing and certifying.
#[test]
fn epochs_follow_one_another() {
    let mut network = Network::with_epoch_duration(3, 25_000);
    let mut previous_last = 0;
    for (epoch, coin) in [(0, 0), (1, 1), (2, 2)] {
        assert_eq!(network.epoch().params.epoch, epoch);
        let record = EpochRecord::load(network.store());
        assert_eq!(record.epoch, epoch);
        assert_eq!(record.previous_checkpoint, previous_last);
        let transfer = network.transfer(network.coin(coin));
        let seq = network.commit(vec![consensus_transaction(&transfer)]);
        assert_eq!(seq, previous_last + 1, "checkpoints continue across epochs");
        let certified = network.certify(seq);
        assert_eq!(certified.epoch, epoch);
        network.assert_checkpointed(seq, &[&transfer]);
        previous_last = network.next_epoch();
        let state = execution::system_state::read(network.store()).unwrap();
        assert_eq!(state.epoch, epoch + 1);
        let root = state_hasher::decode(&EpochRecord::load(network.store()).root_state);
        assert_eq!(network.live_set(previous_last).digest(), root.digest());
    }
    // Each checkpoint follows the stored one before it, across both ends.
    for seq in 1..=previous_last {
        let previous = network.committed(seq - 1);
        let summary: CheckpointSummary = bcs::from_bytes(&network.committed(seq).summary).unwrap();
        assert_eq!(
            summary.previous_digest.unwrap().into_inner(),
            previous.digest
        );
    }
    assert_no_panics();
}

/// A crash after the epoch's last checkpoint commits, before the next
/// epoch's start record: the node starts in the ended epoch, so moves on to
/// the next at once.
#[test]
fn a_crash_between_the_last_checkpoint_and_the_next_epoch_moves_on_at_once() {
    let mut network = Network::with_epoch_duration(2, 25_000);
    let last = network.end_epoch();
    network.crash();
    assert_eq!(EpochRecord::load(network.store()).epoch, 0);
    assert!(node::epoch::epoch_ended(network.store()));
    network.recover();
    assert_eq!(network.epoch().params.epoch, 1);
    let record = EpochRecord::load(network.store());
    assert_eq!((record.epoch, record.previous_checkpoint), (1, last));

    let transfer = network.transfer(network.coin(0));
    let seq = network.commit(vec![consensus_transaction(&transfer)]);
    assert_eq!(seq, last + 1);
    network.certify(seq);
    network.assert_checkpointed(seq, &[&transfer]);
    assert_no_panics();
}

/// A crash in the next epoch, after its start record: the node starts in
/// it, and replays its commits, executing again what had not committed.
#[test]
fn a_crash_in_the_next_epoch_replays_it() {
    let mut network = Network::with_epoch_duration(2, 25_000);
    let last = network.next_epoch();
    let transfer = network.transfer(network.coin(0));
    let seq = network.commit(vec![consensus_transaction(&transfer)]);
    let executed = network.wait_executed(&transfer);

    network.crash();
    assert_eq!(EpochRecord::load(network.store()).epoch, 1);
    assert!(network.executed(&transfer).is_none());
    network.recover();
    assert_eq!(network.epoch().params.epoch, 1);
    assert_eq!(network.wait_executed(&transfer), executed);
    assert_eq!(seq, last + 1);
    network.certify(seq);
    network.assert_checkpointed(seq, &[&transfer]);
    assert_no_panics();
}

/// Submits `transactions` through the pipeline's RPC service as one
/// request; each one's status.
async fn submit(
    service: &Validator,
    transactions: &[&Transaction],
) -> Vec<RawValidatorSubmitStatus> {
    let request = RawSubmitTxRequest {
        transactions: transactions
            .iter()
            .map(|t| bcs::to_bytes(t).unwrap().into())
            .collect(),
        submit_type: SubmitTxType::Default as i32,
    };
    let response = service
        .submit_transaction(tonic::Request::new(request))
        .await
        .unwrap();
    response
        .into_inner()
        .results
        .into_iter()
        .map(|r| r.inner.unwrap())
        .collect()
}

/// The position `status` reports a transaction submitted at.
fn submitted(status: &RawValidatorSubmitStatus) -> ConsensusPosition {
    let RawValidatorSubmitStatus::Submitted(position) = status else {
        panic!("not submitted");
    };
    ConsensusPosition::from_bcs(position).unwrap()
}

/// What the pipeline's RPC service answers a wait for `transaction`'s
/// effects, submitted at `position`.
async fn wait(
    service: &Validator,
    transaction: &Transaction,
    position: Option<ConsensusPosition>,
) -> RawValidatorTransactionStatus {
    let request = RawWaitForEffectsRequest {
        transaction_digest: Some(transaction.digest().into_inner().to_vec().into()),
        consensus_position: position.map(|p| p.to_bcs().into()),
        include_details: false,
        ping_type: None,
    };
    let response = tokio::time::timeout(
        TIMEOUT,
        service.wait_for_effects(tonic::Request::new(request)),
    )
    .await
    .expect("answered")
    .unwrap();
    response.into_inner().inner.unwrap()
}

/// Plays consensus for submissions made on another task: once something is
/// submitted, commits it.
fn commit_submitted(network: &mut Network) -> u64 {
    wait_for("a submission", || {
        network.poll();
        (!network.pending.is_empty()).then_some(())
    });
    network.commit(vec![])
}

/// A wait in flight when its epoch's pipeline stops, and a submission made
/// while none runs, are both answered by the next pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_carry_over_to_the_next_pipeline() {
    let mut network = Network::new(2);
    let service = Arc::new(network.service());
    let first = network.transfer(network.coin(0));
    let waiting = tokio::spawn({
        let (service, first) = (service.clone(), first.clone());
        async move { wait(&service, &first, None).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    network.stop_epoch();

    let second = network.transfer(network.coin(1));
    let submitting = tokio::spawn({
        let (service, second) = (service.clone(), second.clone());
        async move { submit(&service, &[&second]).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished() && !submitting.is_finished());

    network.recover();
    network.commit(vec![consensus_transaction(&first)]);
    let RawValidatorTransactionStatus::Executed(_) = waiting.await.unwrap() else {
        panic!("executed");
    };
    let seq = commit_submitted(&mut network);
    let position = submitted(&submitting.await.unwrap()[0]);
    assert_eq!(u64::from(position.block.round), seq);
    assert_no_panics();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_submission_is_answered_with_its_position_then_its_effects() {
    let mut network = Network::new(1);
    let service = Arc::new(network.service());
    let transfer = network.transfer(network.coin(0));
    let submitting = tokio::spawn({
        let (service, transfer) = (service.clone(), transfer.clone());
        async move { submit(&service, &[&transfer]).await }
    });
    let seq = commit_submitted(&mut network);
    let statuses = submitting.await.unwrap();
    let position = submitted(&statuses[0]);
    assert_eq!(u64::from(position.block.round), seq);
    assert_eq!(position.index, 0);

    let RawValidatorTransactionStatus::Executed(executed) =
        wait(&service, &transfer, Some(position)).await
    else {
        panic!("executed");
    };
    let effects = network.effects(transfer.digest());
    assert_eq!(
        executed.effects_digest,
        bcs::to_bytes(&effects.digest()).unwrap()
    );
    assert_eq!(effects.status(), &ExecutionStatus::Success);

    // Submitted again, it is answered with its effects at once.
    let again = submit(&service, &[&transfer]).await;
    assert!(matches!(again[..], [RawValidatorSubmitStatus::Executed(_)]));
    network.certify(seq);
    network.assert_checkpointed(seq, &[&transfer]);
    assert_no_panics();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transaction_dropped_after_consensus_is_answered_rejected() {
    let mut network = Network::new(1);
    let service = Arc::new(network.service());
    let coin = network.coin(0);
    let (first, second) = (network.transfer(coin), network.transfer(coin));
    let submitting = tokio::spawn({
        let (service, first, second) = (service.clone(), first.clone(), second.clone());
        async move { submit(&service, &[&first, &second]).await }
    });
    let seq = commit_submitted(&mut network);
    let statuses = submitting.await.unwrap();
    let (at_first, at_second) = (submitted(&statuses[0]), submitted(&statuses[1]));

    // The second lost its coin's lock to the first: dropped, never executed.
    assert!(matches!(
        wait(&service, &second, Some(at_second)).await,
        RawValidatorTransactionStatus::Rejected(_)
    ));
    assert!(matches!(
        wait(&service, &first, Some(at_first)).await,
        RawValidatorTransactionStatus::Executed(_)
    ));
    network.certify(seq);
    network.assert_checkpointed(seq, &[&first]);
    assert_no_panics();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_position_never_committed_expires() {
    let mut network = Network::new(1);
    let service = Arc::new(network.service());
    let transfer = network.transfer(network.coin(0));
    let submitting = tokio::spawn({
        let (service, transfer) = (service.clone(), transfer.clone());
        async move { submit(&service, &[&transfer]).await }
    });
    // Consensus includes it in a block it never commits.
    let guard = wait_for("a submission", || {
        network.poll();
        network.pending.pop()
    });
    let lost = BlockRef {
        round: 1,
        author: 1,
        digest: [0xee; 32],
    };
    let (_status, receiver) = oneshot::channel();
    let _ = guard.included_in_block_ack.send((lost, vec![0], receiver));
    let position = submitted(&submitting.await.unwrap()[0]);
    assert_eq!(position.block, lost);

    let waiting = tokio::spawn({
        let (service, transfer) = (service.clone(), transfer.clone());
        async move { wait(&service, &transfer, Some(position)).await }
    });
    // Commits move the leader round past the block's retention.
    let statuses = &network.pipeline().statuses;
    statuses.update_leader_round(1 + RETENTION_ROUNDS);
    statuses.update_leader_round(2 + RETENTION_ROUNDS);
    let RawValidatorTransactionStatus::Expired(expired) = waiting.await.unwrap() else {
        panic!("expired");
    };
    assert_eq!(expired.epoch, 0);
    assert_eq!(expired.round, Some(1 + RETENTION_ROUNDS));
    assert_no_panics();
}

/// Commits fed back to back, without waiting for any to execute: each
/// writes one shared coin twice and reads the clock, among transfers.
#[test]
fn back_to_back_commits_execute_in_order_and_certify() {
    const COMMITS: u64 = 5;
    const TRANSFERS: u64 = 6;
    let per_commit = TRANSFERS + 3;
    let mut network = Network::new((1 + COMMITS * per_commit) as usize);
    let share = network.share(network.coin(0));
    network.commit(vec![consensus_transaction(&share)]);
    let effects = wait_for("the share to execute", || {
        execution::executed(network.store(), &share.digest().into_inner()).unwrap()
    });
    let effects: TransactionEffects = bcs::from_bytes(&effects.effects).unwrap();
    let (shared_ref, _) = effects
        .created()
        .into_iter()
        .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
        .expect("a shared coin");
    let shared = shared_arg(&network.live(shared_ref.0), true);

    let mut committed = vec![(1, vec![share])];
    let mut coin = 1;
    for _ in 0..COMMITS {
        let mut next = || {
            coin += 1;
            network.coin(coin - 1)
        };
        let mut transactions = Vec::new();
        for i in 0..per_commit {
            let gas = next();
            transactions.push(match i {
                0 | 4 => network.split_shared(gas, shared),
                7 => network.read_clock(gas),
                _ => network.transfer(gas),
            });
        }
        let seq = network.commit(transactions.iter().map(consensus_transaction).collect());
        committed.push((seq, transactions));
    }

    let last = committed.last().unwrap().0;
    network.certify(last);
    let mut listed = HashSet::new();
    for seq in 1..=last {
        let summary = network.certify(seq);
        if seq > 1 {
            let previous = network.certify(seq - 1);
            assert_eq!(summary.previous_digest, Some(*previous.digest()), "{seq}");
        }
        for digest in network.contents(seq) {
            assert!(listed.insert(digest), "{digest} listed twice");
        }
    }
    for (seq, transactions) in &committed {
        network.assert_checkpointed(*seq, &transactions.iter().collect::<Vec<_>>());
    }
    // The shared coin lost a MIST to each write.
    let coin: GasCoin = bcs::from_bytes(
        network
            .live(shared_ref.0)
            .data
            .try_as_move()
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(coin.value(), SUI - 2 * COMMITS);
    assert_no_panics();
}

/// Funds sent to an address balance settle, and a later commit withdraws
/// from it.
#[test]
fn funds_sent_to_an_address_balance_settle_and_can_be_withdrawn() {
    let mut network = Network::new(2);
    let send = network.send_funds(network.coin(0), 2 * SUI);
    let seq_1 = network.commit(vec![consensus_transaction(&send)]);
    network.certify(seq_1);
    network.assert_checkpointed(seq_1, &[&send]);
    // The prologue, the transfer, a settlement and the barrier.
    assert_eq!(network.contents(seq_1).len(), 4);

    // This validator's input checks reject funds withdrawals (address
    // balances are not supported there yet); the committee commits it.
    let withdraw = network.withdraw(network.coin(1), SUI);
    let seq_2 = network.commit_outvoted(vec![consensus_transaction(&withdraw)], &[0]);
    network.certify(seq_2);
    network.assert_checkpointed(seq_2, &[&withdraw]);
    let created = network.effects(withdraw.digest()).created();
    assert_eq!(created.len(), 1, "a coin redeemed from the balance");

    assert_no_panics();
}

/// Several commits per checkpoint, by commit time: a checkpoint of several
/// chunks. A transaction committed twice runs once; one that fails still
/// settles.
#[test]
fn checkpoints_of_several_commits() {
    let mut network = Network::new(8);
    network.commit_interval_ms = 50;
    let transfers: Vec<_> = (0..6).map(|i| network.transfer(network.coin(i))).collect();
    // More than the coin holds: fails, charging gas.
    let failing = network.sign(TransactionData::new_transfer_sui(
        SuiAddress::random_for_testing_only(),
        network.sender,
        Some(100 * SUI),
        network.coin(6),
        BUDGET,
        RGP,
    ));
    for pair in transfers.chunks(2) {
        network.commit(pair.iter().map(consensus_transaction).collect());
    }
    network.commit(vec![
        consensus_transaction(&transfers[0]),
        consensus_transaction(&failing),
    ]);
    // Commits until checkpoint 2 is cut and built.
    wait_for("checkpoint 2 to be built", || {
        network.commit(vec![]);
        network.pipeline().checkpoints.is_built(2).then_some(())
    });
    network.certify(2);
    let mut listed = HashSet::new();
    for seq in 1..=2 {
        for digest in network.contents(seq) {
            assert!(listed.insert(digest), "{digest} listed twice");
        }
    }
    for transfer in &transfers {
        assert!(listed.contains(transfer.digest()));
    }
    assert!(listed.contains(failing.digest()));
    assert!(matches!(
        network.effects(failing.digest()).status(),
        ExecutionStatus::Failure { .. }
    ));
    assert_no_panics();
}

/// Two transactions paying with the same coin version, in one commit: the
/// later one in consensus order is dropped.
#[test]
fn an_owned_conflict_in_one_commit_drops_the_later_transaction() {
    let mut network = Network::new(1);
    let coin = network.coin(0);
    let first = network.transfer(coin);
    let second = network.transfer(coin);
    let seq = network.commit(vec![
        consensus_transaction(&first),
        consensus_transaction(&second),
    ]);
    network.certify(seq);
    network.assert_checkpointed(seq, &[&first]);
    assert!(!network.contents(seq).contains(second.digest()));
    assert!(
        execution::executed(network.store(), &second.digest().into_inner())
            .unwrap()
            .is_none()
    );
    assert_no_panics();
}

/// A withdrawal of more than the balance its commit reserves against fails,
/// even with a deposit earlier in the commit, which settles only after it.
#[test]
fn a_withdrawal_exceeding_the_settled_balance_fails() {
    let mut network = Network::new(3);
    let send = network.send_funds(network.coin(0), SUI);
    let seq = network.commit(vec![consensus_transaction(&send)]);
    network.certify(seq);
    // This validator's input checks reject funds withdrawals; the committee
    // commits it.
    let deposit = network.send_funds(network.coin(1), 5 * SUI);
    let overdraw = network.withdraw(network.coin(2), 2 * SUI);
    let seq = network.commit_outvoted(
        vec![
            consensus_transaction(&deposit),
            consensus_transaction(&overdraw),
        ],
        &[1],
    );
    network.certify(seq);
    network.assert_checkpointed(seq, &[&deposit]);
    let effects = network.effects(overdraw.digest());
    // Its reservation was cancelled, so it failed without running.
    let ExecutionStatus::Failure(failure) = effects.status() else {
        panic!("{:?}", effects.status());
    };
    assert_eq!(
        failure.error,
        ExecutionErrorKind::InsufficientFundsForWithdraw
    );
    assert!(network.contents(seq).contains(overdraw.digest()));
    assert_no_panics();
}
