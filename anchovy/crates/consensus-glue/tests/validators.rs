// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validators on consensus-core, in one process over localhost: each node
//! runs epoch after epoch, transactions submitted through any one's RPC
//! service execute on all, every validator certifies the same checkpoints,
//! and after a crash consensus replays from its database and catches up
//! from its peers.

#[path = "../../node/tests/genesis/mod.rs"]
mod genesis;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use consensus_glue::{Parameters, Runner};
use messages::base::{Digest, ObjectId};
use node::epoch::{EpochRecord, Node, Perpetual};
use protocol_config::Chain;
use sui_types::base_types::{AuthorityName, ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
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

/// A port of localhost nothing listens on, for now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Not waiting at start for peers to return the validator's last block:
/// its database survives its crashes.
fn parameters() -> Parameters {
    Parameters {
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

struct Member {
    index: u8,
    /// None while crashed.
    running: Option<Running>,
    /// State sync's end, kept across crashes.
    state_sync: (Queue<CertifiedCheckpoint>, Inbox<CertifiedCheckpoint>),
    certified: BTreeMap<u64, CertifiedCheckpointSummary>,
    // Dropped after the node.
    dir: tempfile::TempDir,
}

impl Member {
    fn running(&self) -> &Running {
        self.running.as_ref().expect("the node is running")
    }

    fn store(&self) -> &store::Store {
        &self.running().store
    }

    /// Whether `transaction` has executed here.
    fn executed(&self, transaction: &Transaction) -> bool {
        execution::executed(self.store(), &transaction.digest().into_inner())
            .unwrap()
            .is_some()
    }

    /// The checkpoint listing `transaction`, if committed and certified.
    fn checkpoint_of(&self, transaction: &Transaction) -> Option<u64> {
        let digest = *transaction.digest();
        let committed = self.store().committed_checkpoint().unwrap().unwrap();
        (1..=committed).find(|seq| {
            let built = BuiltCheckpoint::decode(&self.store().checkpoint(*seq).unwrap().unwrap());
            let contents: CheckpointContents = bcs::from_bytes(&built.contents).unwrap();
            contents.iter().any(|d| d.transaction == digest) && self.certified.contains_key(seq)
        })
    }

    /// The epoch of the last checkpoint certified.
    fn epoch(&self) -> u64 {
        self.certified.values().last().map_or(0, |s| s.epoch)
    }
}

struct Network {
    runtime: Runtime,
    members: Vec<Member>,
    /// Every epoch's committee: the validator set never changes.
    stakes: BTreeMap<AuthorityName, u64>,
    chain_identifier: Digest,
    sender: SuiAddress,
    key: AccountKeyPair,
    coins: Vec<ObjectID>,
}

impl Network {
    fn new(validators: u8, coins: usize, epoch_duration_ms: u64) -> Network {
        record_panics();
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let ports: Vec<u16> = (0..validators).map(|_| free_port()).collect();
        let genesis = genesis::build_with(&ports, sender, coins, 10 * SUI, epoch_duration_ms);
        let mut members = Vec::new();
        let mut stakes = BTreeMap::new();
        for index in 0..validators {
            let dir = tempfile::tempdir().unwrap();
            let store = store::Store::open(&dir.path().join("perpetual")).unwrap();
            execution::genesis::commit(&store, &genesis.objects).unwrap();
            node::epoch::genesis(
                &store,
                &genesis.checkpoint,
                genesis.objects.iter().map(|o| o.digest().into_inner()),
            );
            stakes = EpochRecord::load(&store)
                .stakes()
                .map(|(name, stake)| (AuthorityName::new(name), stake))
                .collect();
            members.push(Member {
                index,
                running: None,
                state_sync: workqueue::queue(1024),
                certified: BTreeMap::new(),
                dir,
            });
        }
        let mut network = Network {
            runtime: Runtime::new().unwrap(),
            members,
            stakes,
            chain_identifier: Digest::new(genesis.chain_identifier),
            sender,
            key,
            coins: genesis.coins,
        };
        for v in 0..usize::from(validators) {
            network.start(v);
        }
        network
    }

    /// Runs validator `v`'s node over its store, consensus replaying from
    /// its database.
    fn start(&mut self, v: usize) {
        let member = &mut self.members[v];
        assert!(member.running.is_none());
        // Waits for a crashed node's store to close.
        let store = Arc::new(store::Store::open(&member.dir.path().join("perpetual")).unwrap());
        let current = CurrentEpoch::new();
        let keys = genesis::validator_keys_of(member.index);
        let runner = Runner::new(&keys, parameters());
        let perpetual = Perpetual {
            store: store.clone(),
            epochs: member.dir.path().join("epochs"),
            current: current.clone(),
            state_sync: member.state_sync.0.clone(),
            chain: Chain::Unknown,
            chain_identifier: self.chain_identifier,
            keys,
        };
        let task = self.runtime.spawn(Node::new(perpetual).run(runner));
        member.running = Some(Running {
            store,
            current,
            task,
        });
    }

    /// Stops validator `v`'s node wherever it is.
    fn crash(&mut self, v: usize) {
        let member = &mut self.members[v];
        let running = member.running.take().unwrap();
        running.task.abort();
        let stopped = self.runtime.block_on(running.task);
        assert!(stopped.unwrap_err().is_cancelled(), "the node was running");
        // Checkpoints certified and not committed are certified again.
        let committed = running.store.committed_checkpoint().unwrap().unwrap();
        member.certified.retain(|seq, _| *seq <= committed);
    }

    fn running(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.running.is_some())
    }

    /// Takes what each validator handed state sync, checking each is signed
    /// by a quorum.
    fn poll(&mut self) {
        for member in &mut self.members {
            while let Some(certified) = member.state_sync.1.try_pop() {
                let summary: CertifiedCheckpointSummary =
                    bcs::from_bytes(&certified.summary).unwrap();
                assert_eq!(summary.sequence_number, certified.seq);
                let committee =
                    sui_types::committee::Committee::new(summary.epoch, self.stakes.clone());
                summary.verify_authority_signatures(&committee).unwrap();
                if let Some(before) = member.certified.insert(certified.seq, summary.clone()) {
                    assert_eq!(before.digest(), summary.digest(), "{} again", certified.seq);
                }
            }
        }
    }

    /// Waits until `f` gives something, polling.
    fn wait_for<T>(&mut self, what: &str, mut f: impl FnMut(&Network) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.poll();
            if let Some(t) = f(self) {
                return t;
            }
            assert_no_panics();
            for member in self.running() {
                assert!(
                    !member.running().task.is_finished(),
                    "validator {} stopped",
                    member.index
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:#?}",
                self.status()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Each running validator's epoch, committed checkpoint and last
    /// certified checkpoint.
    fn status(&self) -> Vec<String> {
        self.running()
            .map(|m| {
                format!(
                    "validator {}: epoch {}, committed {:?}, certified {:?}",
                    m.index,
                    EpochRecord::load(m.store()).epoch,
                    m.store().committed_checkpoint().unwrap(),
                    m.certified.keys().last(),
                )
            })
            .collect()
    }

    /// Coin `i` as validator 0 has it.
    fn coin(&self, i: usize) -> ObjectRef {
        let object = self.members[0]
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

    /// Submits `transaction` through validator `v`'s RPC service and waits
    /// for its effects there.
    fn execute(&self, v: usize, transaction: &Transaction) -> TransactionEffects {
        let member = &self.members[v];
        let service = Validator::over(member.running().current.clone());
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
        let executed = execution::executed(member.store(), &transaction.digest().into_inner())
            .unwrap()
            .expect("executed");
        let effects: TransactionEffects = bcs::from_bytes(&executed.effects).unwrap();
        assert_eq!(effects.status(), &ExecutionStatus::Success);
        effects
    }

    /// Waits until every running validator has executed `transaction` and
    /// certified its checkpoint, the same one; its sequence number.
    fn checkpointed(&mut self, transaction: &Transaction) -> u64 {
        self.wait_for("the transaction's checkpoint everywhere", |network| {
            let seqs: Vec<Option<u64>> = network
                .running()
                .map(|m| {
                    m.executed(transaction)
                        .then(|| m.checkpoint_of(transaction))
                        .flatten()
                })
                .collect();
            let first = seqs[0]?;
            seqs.iter().all(|s| *s == Some(first)).then_some(first)
        })
    }

    /// Asserts every validator certified the same checkpoints, each
    /// chaining to the one before.
    fn assert_agreed(&self) {
        let mut all = BTreeMap::new();
        for member in &self.members {
            for (seq, summary) in &member.certified {
                let digest = *all.entry(*seq).or_insert(*summary.digest());
                assert_eq!(&digest, summary.digest(), "{seq} on {}", member.index);
            }
        }
        for member in &self.members {
            for (seq, summary) in member.certified.iter().skip(1) {
                if let Some(previous) = all.get(&(seq - 1)) {
                    assert_eq!(summary.previous_digest, Some(*previous), "{seq}");
                }
            }
        }
    }

    /// Asserts each epoch's last checkpoint, as validator `v` certified,
    /// closes it.
    fn assert_epochs_closed(&self, v: usize) {
        let summaries: Vec<_> = self.members[v].certified.values().collect();
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
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        for member in &mut self.members {
            if let Some(running) = member.running.take() {
                running.task.abort();
                let _ = self.runtime.block_on(running.task);
            }
        }
    }
}

#[test]
fn one_validator_executes_certifies_and_resumes_after_a_crash() {
    let mut network = Network::new(1, 2, FOREVER_MS);
    let first = network.transfer(0);
    network.execute(0, &first);
    let first_seq = network.checkpointed(&first);

    network.crash(0);
    network.start(0);
    // Spending the first transfer's output: its effects survived.
    let second = network.transfer(0);
    network.execute(0, &second);
    assert!(network.checkpointed(&second) > first_seq);
    let other = network.transfer(1);
    network.execute(0, &other);
    network.checkpointed(&other);
    network.assert_agreed();
    assert!(network.members[0].certified.values().all(|s| s.epoch == 0));
    assert_no_panics();
}

#[test]
fn one_validator_moves_through_epochs_across_a_crash() {
    // The first epoch, starting at time 0, ends at the first commit.
    let mut network = Network::new(1, 1, 2_000);
    network.wait_for("epoch 2", |n| (n.members[0].epoch() >= 2).then_some(()));
    network.crash(0);
    network.start(0);
    network.wait_for("epoch 4", |n| (n.members[0].epoch() >= 4).then_some(()));
    let transfer = network.transfer(0);
    network.execute(0, &transfer);
    network.checkpointed(&transfer);
    network.assert_epochs_closed(0);
    network.assert_agreed();
    assert_no_panics();
}

#[test]
fn four_validators_agree_and_a_crashed_one_catches_up() {
    let mut network = Network::new(4, 3, FOREVER_MS);
    let first = network.transfer(0);
    network.execute(0, &first);
    network.checkpointed(&first);

    // Three of four are a quorum.
    network.crash(3);
    let second = network.transfer(1);
    network.execute(1, &second);
    network.checkpointed(&second);

    network.start(3);
    let third = network.transfer(2);
    network.execute(3, &third);
    network.checkpointed(&third);
    assert!(network.members[3].executed(&second));
    network.assert_agreed();
    assert_no_panics();
}

/// A validator still short of its epoch's last certified checkpoint when a
/// quorum of its peers has moved on and stopped that epoch's consensus can
/// finish the epoch only through state sync, which does not hand the
/// pipeline certified checkpoints yet. So this passes when the validators
/// keep pace, as on an idle machine, and hangs otherwise.
#[test]
#[ignore = "needs certified checkpoints from state sync"]
fn four_validators_move_through_epochs_together() {
    let mut network = Network::new(4, 1, 3_000);
    network.wait_for("epoch 3 everywhere", |n| {
        n.members.iter().all(|m| m.epoch() >= 3).then_some(())
    });
    let transfer = network.transfer(0);
    network.execute(2, &transfer);
    network.checkpointed(&transfer);
    for v in 0..4 {
        network.assert_epochs_closed(v);
    }
    network.assert_agreed();
    assert_no_panics();
}
