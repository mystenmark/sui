// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! One validator on consensus-core: the node runs epoch after epoch over a
//! committee of one, transactions submitted through its RPC service execute
//! and their checkpoints certify, and after a crash consensus replays from
//! its database.

#[path = "../../node/tests/genesis/mod.rs"]
mod genesis;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use consensus_glue::{Parameters, Runner};
use messages::base::{Digest, ObjectId};
use node::epoch::{Node, Perpetual};
use protocol_config::Chain;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, AuthorityPublicKeyBytes, get_key_pair};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::execution_status::ExecutionStatus;
use sui_types::messages_checkpoint::{CertifiedCheckpointSummary, CheckpointContents};
use sui_types::object::Object;
use sui_types::transaction::{Transaction, TransactionData};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use validator::Validator;
use validator::current::CurrentEpoch;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::BuiltCheckpoint;
use validator::proto::{
    RawSubmitTxRequest, RawValidatorSubmitStatus, RawValidatorTransactionStatus,
    RawWaitForEffectsRequest, SubmitTxType,
};
use validator::service::validator_server::Validator as _;
use workqueue::{Inbox, Queue};

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;
const TIMEOUT: Duration = Duration::from_mins(1);
/// Longer than any test: commits carry the wall clock, and genesis starts
/// the first epoch at time 0.
const FOREVER_MS: u64 = 1 << 50;

static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Workers catch an item's panic and carry on, so a passing assertion could
/// otherwise hide one.
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
    let panics = PANICS.lock().unwrap().clone();
    assert!(panics.is_empty(), "panics: {panics:#?}");
}

/// Listening on any free port, and not waiting at start for peers to
/// return its last block: it has none.
fn parameters() -> Parameters {
    Parameters {
        listen_address_override: Some("/ip4/127.0.0.1/tcp/0".parse().unwrap()),
        sync_last_known_own_block_timeout: Duration::ZERO,
        ..Parameters::default()
    }
}

/// A node run by `Node::run` on consensus-core.
struct Running {
    store: Arc<store::Store>,
    current: CurrentEpoch,
    task: JoinHandle<()>,
}

struct Harness {
    runtime: Runtime,
    running: Option<Running>,
    /// State sync's end, kept across crashes.
    state_sync: (Queue<CertifiedCheckpoint>, Inbox<CertifiedCheckpoint>),
    certified: BTreeMap<u64, CertifiedCheckpointSummary>,
    chain_identifier: Digest,
    sender: SuiAddress,
    key: AccountKeyPair,
    coins: Vec<ObjectID>,
    // Dropped after the node.
    dir: tempfile::TempDir,
}

impl Harness {
    fn new(coins: usize, epoch_duration_ms: u64) -> Harness {
        record_panics();
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::open(&dir.path().join("perpetual")).unwrap();
        let genesis = genesis::build(sender, coins, 10 * SUI, epoch_duration_ms);
        execution::genesis::commit(&store, &genesis.objects).unwrap();
        node::epoch::genesis(
            &store,
            &genesis.checkpoint,
            genesis.objects.iter().map(|o| o.digest().into_inner()),
        );
        drop(store);
        let mut harness = Harness {
            runtime: Runtime::new().unwrap(),
            running: None,
            state_sync: workqueue::queue(1024),
            certified: BTreeMap::new(),
            chain_identifier: Digest::new(genesis.chain_identifier),
            sender,
            key,
            coins: genesis.coins,
            dir,
        };
        harness.start();
        harness
    }

    /// Runs the node over the store, consensus replaying from its database.
    fn start(&mut self) {
        assert!(self.running.is_none());
        // Waits for a crashed node's store to close.
        let store = Arc::new(store::Store::open(&self.dir.path().join("perpetual")).unwrap());
        let current = CurrentEpoch::new();
        let keys = genesis::validator_keys();
        let runner = Runner::new(&keys, parameters());
        let perpetual = Perpetual {
            store: store.clone(),
            epochs: self.dir.path().join("epochs"),
            current: current.clone(),
            state_sync: self.state_sync.0.clone(),
            chain: Chain::Unknown,
            chain_identifier: self.chain_identifier,
            keys,
        };
        let task = self.runtime.spawn(Node::new(perpetual).run(runner));
        self.running = Some(Running {
            store,
            current,
            task,
        });
    }

    /// Stops the node wherever it is.
    fn crash(&mut self) {
        let running = self.running.take().unwrap();
        running.task.abort();
        let stopped = self.runtime.block_on(running.task);
        assert!(stopped.unwrap_err().is_cancelled(), "the node was running");
        // Checkpoints certified and not committed are certified again.
        let committed = running.store.committed_checkpoint().unwrap().unwrap();
        self.certified.retain(|seq, _| *seq <= committed);
    }

    fn running(&self) -> &Running {
        self.running.as_ref().expect("the node is running")
    }

    fn store(&self) -> &store::Store {
        &self.running().store
    }

    fn service(&self) -> Validator {
        Validator::over(self.running().current.clone())
    }

    /// Takes what state sync was handed, checking each is signed by the
    /// validator.
    fn poll(&mut self) {
        let name = AuthorityPublicKeyBytes::new(*genesis::validator_keys().protocol.public());
        while let Some(certified) = self.state_sync.1.try_pop() {
            let summary: CertifiedCheckpointSummary = bcs::from_bytes(&certified.summary).unwrap();
            assert_eq!(summary.sequence_number, certified.seq);
            let committee = sui_types::committee::Committee::new(
                summary.epoch,
                BTreeMap::from([(name, 10_000)]),
            );
            summary.verify_authority_signatures(&committee).unwrap();
            if let Some(before) = self.certified.insert(certified.seq, summary.clone()) {
                assert_eq!(before.digest(), summary.digest(), "{} again", certified.seq);
            }
        }
    }

    /// Waits until `f` gives something, polling.
    fn wait_for<T>(&mut self, what: &str, mut f: impl FnMut(&mut Harness) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.poll();
            if let Some(t) = f(self) {
                return t;
            }
            assert_no_panics();
            assert!(!self.running().task.is_finished(), "the node stopped");
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn coin(&self, i: usize) -> ObjectRef {
        let object = self
            .store()
            .live_object(&ObjectId(self.coins[i].into_bytes()))
            .unwrap()
            .unwrap();
        let object: Object = bcs::from_bytes(object.wire_bytes()).unwrap();
        object.compute_object_reference()
    }

    /// 1 SUI to a fresh address, paid with coin `i`.
    fn transfer(&self, i: usize) -> Transaction {
        let data = TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            self.coin(i),
            BUDGET,
            RGP,
        );
        Transaction::from_data_and_signer(data, vec![&self.key])
    }

    /// Submits `transaction` through the RPC service and waits for its
    /// effects.
    fn execute(&self, transaction: &Transaction) -> TransactionEffects {
        let service = self.service();
        self.runtime.block_on(async {
            let request = RawSubmitTxRequest {
                transactions: vec![bcs::to_bytes(transaction).unwrap().into()],
                submit_type: SubmitTxType::Default as i32,
            };
            let submitted = tokio::time::timeout(
                TIMEOUT,
                service.submit_transaction(tonic::Request::new(request)),
            )
            .await
            .expect("submitted in time")
            .unwrap();
            let position = match submitted.into_inner().results.remove(0).inner.unwrap() {
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
            let status = tokio::time::timeout(
                TIMEOUT,
                service.wait_for_effects(tonic::Request::new(request)),
            )
            .await
            .expect("answered in time")
            .unwrap();
            let RawValidatorTransactionStatus::Executed(_) = status.into_inner().inner.unwrap()
            else {
                panic!("executed");
            };
        });
        let executed = execution::executed(self.store(), &transaction.digest().into_inner())
            .unwrap()
            .expect("executed");
        let effects: TransactionEffects = bcs::from_bytes(&executed.effects).unwrap();
        assert_eq!(effects.status(), &ExecutionStatus::Success);
        effects
    }

    /// The certified checkpoint listing `transaction`, once it has
    /// committed.
    fn checkpoint_of(&mut self, transaction: &Transaction) -> u64 {
        let digest = *transaction.digest();
        self.wait_for("the transaction's checkpoint", |h| {
            let committed = h.store().committed_checkpoint().unwrap().unwrap();
            (1..=committed).find(|seq| {
                let built = BuiltCheckpoint::decode(&h.store().checkpoint(*seq).unwrap().unwrap());
                let contents: CheckpointContents = bcs::from_bytes(&built.contents).unwrap();
                contents.iter().any(|d| d.transaction == digest) && h.certified.contains_key(seq)
            })
        })
    }

    /// Asserts the certified checkpoints chain, each to the one before.
    fn assert_chained(&self) {
        for (seq, summary) in self.certified.iter().skip(1) {
            if let Some(previous) = self.certified.get(&(seq - 1)) {
                assert_eq!(summary.previous_digest, Some(*previous.digest()), "{seq}");
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            running.task.abort();
            let _ = self.runtime.block_on(running.task);
        }
    }
}

#[test]
fn transactions_execute_and_certify_and_a_crash_resumes() {
    let mut harness = Harness::new(2, FOREVER_MS);
    let first = harness.transfer(0);
    harness.execute(&first);
    let first_seq = harness.checkpoint_of(&first);

    harness.crash();
    harness.start();
    // Spending the first transfer's output: its effects survived.
    let second = harness.transfer(0);
    harness.execute(&second);
    let second_seq = harness.checkpoint_of(&second);
    assert!(second_seq > first_seq);
    let other = harness.transfer(1);
    harness.execute(&other);
    harness.checkpoint_of(&other);
    harness.assert_chained();
    assert!(harness.certified.values().all(|s| s.epoch == 0));
    assert_no_panics();
}

#[test]
fn epochs_follow_one_another_across_a_crash() {
    // The first epoch, starting at time 0, ends at the first commit.
    let mut harness = Harness::new(1, 2_000);
    let epoch = |h: &Harness| h.certified.values().last().map_or(0, |s| s.epoch);
    harness.wait_for("epoch 2", |h| (epoch(h) >= 2).then_some(()));
    harness.crash();
    harness.start();
    harness.wait_for("epoch 4", |h| (epoch(h) >= 4).then_some(()));
    let transfer = harness.transfer(0);
    harness.execute(&transfer);
    harness.checkpoint_of(&transfer);

    // Each epoch's last checkpoint closes it.
    let summaries: Vec<_> = harness.certified.values().collect();
    for pair in summaries.windows(2) {
        if pair[1].epoch != pair[0].epoch {
            assert_eq!(pair[1].epoch, pair[0].epoch + 1);
            assert!(
                pair[0].end_of_epoch_data.is_some(),
                "{}",
                pair[0].sequence_number
            );
        }
    }
    harness.assert_chained();
    assert_no_panics();
}
