// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The writeback cache against a model: random writes, checkpoint commits,
//! bypass writes, reopens and every read, with capacities small enough that
//! eviction runs constantly and large enough that it never does. And readers
//! racing a writer and committer never see an answer go back in time.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::checkpoint::CheckpointData;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use store::{Capacity, Commit, Executed, Live, Removal, Removed, Store, Written};

fn checkpoint() -> Message<CheckpointData<'static>> {
    let mut bytes = include_bytes!("../../messages/tests/data/mainnet-325300367.chk").to_vec();
    bytes.remove(0);
    Message::parse(bytes).map_err(|(e, _)| e).unwrap()
}

/// Real objects, transactions and effects, used under made-up ids and
/// digests: the cache never checks one against the other.
struct Pool {
    objects: Vec<Vec<u8>>,
    transactions: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Pool {
    fn new() -> Pool {
        let checkpoint = checkpoint();
        let txs = &checkpoint.get().transactions;
        Pool {
            objects: txs
                .iter()
                .flat_map(|tx| tx.output_objects.iter())
                .map(|o| o.bytes.to_vec())
                .collect(),
            transactions: txs
                .iter()
                .map(|tx| (tx.transaction.bytes().to_vec(), tx.effects.bytes.to_vec()))
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// An index into the pool's objects.
    Object(usize),
    Deleted,
    Wrapped,
}

fn object_digest(version: u64, index: usize) -> Digest {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&version.to_le_bytes());
    bytes[8..16].copy_from_slice(&(index as u64).to_le_bytes());
    Digest::new(bytes)
}

fn digest(tag: u8, n: u64) -> Digest {
    let mut bytes = [tag; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    Digest::new(bytes)
}

fn id(n: usize) -> ObjectId {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&(n as u64).to_le_bytes());
    ObjectId(bytes)
}

/// A transaction as the model keeps it.
#[derive(Clone)]
struct Tx {
    digest: Digest,
    effects_digest: Digest,
    pool: usize,
    outputs: Vec<(usize, u64, State)>,
}

impl Tx {
    fn commit(&self, pool: &Pool) -> Commit {
        let mut commit = Commit::default();
        for &(object, version, state) in &self.outputs {
            match state {
                State::Object(index) => commit.written.push(Written {
                    id: id(object),
                    version,
                    digest: object_digest(version, index),
                    bytes: pool.objects[index].clone(),
                }),
                State::Deleted | State::Wrapped => commit.removed.push(Removed {
                    id: id(object),
                    version,
                    removal: if state == State::Deleted {
                        Removal::Deleted
                    } else {
                        Removal::Wrapped
                    },
                }),
            }
        }
        let (transaction, effects) = pool.transactions[self.pool].clone();
        commit.executed = Some(Executed {
            digest: self.digest,
            transaction,
            effects_digest: self.effects_digest,
            effects,
            events: None,
        });
        commit
    }
}

#[derive(Default)]
struct Model {
    /// Committed, then uncommitted.
    committed: HashMap<usize, BTreeMap<u64, State>>,
    uncommitted: HashMap<usize, BTreeMap<u64, State>>,
    committed_txs: HashMap<Digest, Tx>,
    /// In write order.
    uncommitted_txs: Vec<Tx>,
}

impl Model {
    fn versions(&self, object: usize) -> BTreeMap<u64, State> {
        let mut all = self.committed.get(&object).cloned().unwrap_or_default();
        all.extend(self.uncommitted.get(&object).cloned().unwrap_or_default());
        all
    }

    fn tx(&self, digest: &Digest) -> Option<&Tx> {
        self.committed_txs
            .get(digest)
            .or_else(|| self.uncommitted_txs.iter().find(|t| &t.digest == digest))
    }

    fn drop_uncommitted(&mut self) {
        self.uncommitted.clear();
        self.uncommitted_txs.clear();
    }
}

const OBJECTS: usize = 200;
/// Objects whose versions readers race.
const RACED: usize = 4;
/// Transactions whose execution readers race.
const TXS: u64 = 20_000;

#[allow(
    clippy::too_many_lines,
    reason = "one match over the model's operations"
)]
fn run_model(seed: u64, capacity: Capacity) {
    let pool = Pool::new();
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_with(dir.path(), capacity).unwrap();
    let mut model = Model::default();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut next_tx = 0;
    let mut next_checkpoint = 0;

    for step in 0..20_000 {
        match rng.gen_range(0..100) {
            // A transaction's outputs.
            0..30 => {
                let mut outputs = Vec::new();
                for _ in 0..rng.gen_range(1..=3) {
                    let object = rng.gen_range(0..OBJECTS);
                    if outputs.iter().any(|&(o, _, _)| o == object) {
                        continue;
                    }
                    let highest = model.versions(object).keys().last().copied().unwrap_or(0);
                    let version = highest + rng.gen_range(1..4);
                    let state = match rng.gen_range(0..10) {
                        0 => State::Deleted,
                        1 => State::Wrapped,
                        _ => State::Object(rng.gen_range(0..pool.objects.len())),
                    };
                    outputs.push((object, version, state));
                }
                let tx = Tx {
                    digest: digest(1, next_tx),
                    effects_digest: digest(2, next_tx),
                    pool: rng.gen_range(0..pool.transactions.len()),
                    outputs,
                };
                next_tx += 1;
                store.write(tx.commit(&pool));
                for &(object, version, state) in &tx.outputs {
                    model
                        .uncommitted
                        .entry(object)
                        .or_default()
                        .insert(version, state);
                }
                model.uncommitted_txs.push(tx);
            }
            // A checkpoint of the oldest uncommitted transactions; sometimes
            // naming one committed already, as a re-commit after a crash does.
            30..40 => {
                let n = rng.gen_range(0..=model.uncommitted_txs.len().min(5));
                let txs: Vec<Tx> = model.uncommitted_txs.drain(..n).collect();
                let mut digests: Vec<Digest> = txs.iter().map(|t| t.digest).collect();
                if let Some(old) = model.committed_txs.keys().next()
                    && rng.gen_bool(0.2)
                {
                    digests.insert(0, *old);
                }
                store.commit_checkpoint(next_checkpoint, &digests, vec![step as u8]);
                next_checkpoint += 1;
                for tx in txs {
                    for &(object, version, state) in &tx.outputs {
                        model.uncommitted.get_mut(&object).unwrap().remove(&version);
                        model
                            .committed
                            .entry(object)
                            .or_default()
                            .insert(version, state);
                    }
                    model.committed_txs.insert(tx.digest, tx);
                }
            }
            // A bypass write, of an object with nothing uncommitted.
            40..42 => {
                let object = rng.gen_range(0..OBJECTS);
                if model
                    .uncommitted
                    .get(&object)
                    .is_some_and(|v| !v.is_empty())
                {
                    continue;
                }
                let highest = model.versions(object).keys().last().copied().unwrap_or(0);
                let index = rng.gen_range(0..pool.objects.len());
                let tx = Tx {
                    digest: digest(3, step),
                    effects_digest: digest(4, step),
                    pool: 0,
                    outputs: vec![(object, highest + 1, State::Object(index))],
                };
                store.write_through(tx.commit(&pool)).unwrap();
                model
                    .committed
                    .entry(object)
                    .or_default()
                    .insert(highest + 1, State::Object(index));
                model.committed_txs.insert(tx.digest, tx);
            }
            // Uncommitted work lost, as on a restart.
            42 => {
                if rng.gen_bool(0.5) {
                    drop(store);
                    store = Store::open_with(dir.path(), capacity).unwrap();
                } else {
                    store.clear_uncommitted();
                }
                model.drop_uncommitted();
                assert_eq!(store.uncommitted_transactions(), 0);
                assert_eq!(
                    store.committed_checkpoint().unwrap(),
                    next_checkpoint.checked_sub(1)
                );
            }
            // Reads.
            _ => {
                let object = rng.gen_range(0..OBJECTS);
                check_object(&store, &model, &pool, object, &mut rng);
                let n = rng.gen_range(0..next_tx + 2);
                check_tx(&store, &model, &pool, &digest(1, n), &digest(2, n));
            }
        }
    }
    for object in 0..OBJECTS {
        check_object(&store, &model, &pool, object, &mut rng);
    }
}

fn check_object(store: &Store, model: &Model, pool: &Pool, object: usize, rng: &mut StdRng) {
    let versions = model.versions(object);
    let id = id(object);
    let bytes = |state: Option<&State>| match state {
        Some(State::Object(i)) => Some(pool.objects[*i].as_slice()),
        _ => None,
    };
    let highest = versions.iter().next_back();
    assert_eq!(
        store.live(&id).unwrap(),
        match highest {
            Some((&version, &State::Object(index))) => Some(Live {
                version,
                digest: object_digest(version, index),
            }),
            _ => None,
        },
        "live {object}"
    );
    assert_eq!(
        store
            .live_object(&id)
            .unwrap()
            .as_ref()
            .map(|o| o.get().bytes),
        bytes(highest.map(|(_, s)| s)),
        "live object {object}"
    );
    let top = highest.map_or(0, |(v, _)| *v) + 3;
    for _ in 0..3 {
        let version = rng.gen_range(0..top);
        assert_eq!(
            store
                .object(&id, version)
                .unwrap()
                .as_ref()
                .map(|o| o.get().bytes),
            bytes(versions.get(&version)),
            "object {object} at {version}"
        );
        assert_eq!(
            store
                .object_at_or_before(&id, version)
                .unwrap()
                .as_ref()
                .map(|o| o.get().bytes),
            bytes(versions.range(..=version).next_back().map(|(_, s)| s)),
            "object {object} at or before {version}"
        );
    }
}

fn check_tx(store: &Store, model: &Model, pool: &Pool, digest: &Digest, effects: &Digest) {
    let tx = model.tx(digest);
    assert_eq!(
        store.executed_effects(digest).unwrap(),
        tx.map(|t| t.effects_digest)
    );
    assert_eq!(
        store
            .transaction(digest)
            .unwrap()
            .map(|t| t.wire_bytes().to_vec()),
        tx.map(|t| pool.transactions[t.pool].0.clone())
    );
    assert_eq!(
        store
            .effects(effects)
            .unwrap()
            .map(|e| e.get().bytes.to_vec()),
        tx.map(|t| pool.transactions[t.pool].1.clone())
    );
}

#[test]
fn reads_match_the_model_with_constant_eviction() {
    for seed in 0..4 {
        run_model(
            seed,
            Capacity {
                objects: 1,
                transactions: 1,
            },
        );
    }
}

#[test]
fn reads_match_the_model_without_eviction() {
    for seed in 10..14 {
        run_model(seed, Capacity::default());
    }
}

/// A writer writes and commits versions of a few objects; readers must see
/// each object's live version only ever go up, so no reader fills the cache
/// with an answer a write or commit already outdated.
#[test]
fn racing_readers_never_see_an_older_version() {
    let pool = Arc::new(Pool::new());
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open_with(
            dir.path(),
            Capacity {
                objects: 1,
                transactions: 1,
            },
        )
        .unwrap(),
    );
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (store, done) = (store.clone(), done.clone());
            std::thread::spawn(move || {
                let mut seen = [0u64; RACED];
                while !done.load(Ordering::Relaxed) {
                    for (object, seen) in seen.iter_mut().enumerate() {
                        let live = store.live(&id(object)).unwrap().map_or(0, |l| l.version);
                        assert!(live >= *seen, "{object}: {live} after {seen}");
                        *seen = live;
                        if let Some(object) =
                            store.object_at_or_before(&id(object), u64::MAX).unwrap()
                        {
                            assert!(!object.get().bytes.is_empty());
                        }
                    }
                }
            })
        })
        .collect();
    let mut pending = Vec::new();
    for n in 0..20_000u64 {
        let object = n as usize % RACED;
        let tx = Tx {
            digest: digest(1, n),
            effects_digest: digest(2, n),
            pool: 0,
            outputs: vec![(
                object,
                n + 1,
                State::Object(n as usize % pool.objects.len()),
            )],
        };
        store.write(tx.commit(&pool));
        pending.push(tx.digest);
        if n % 3 == 2 {
            store.commit_checkpoint(n / 3, &pending, vec![]);
            pending.clear();
        }
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
}

/// Once a reader sees a transaction executed, it stays executed, through its
/// commit and the eviction of its cached record.
#[test]
fn racing_readers_never_see_an_executed_transaction_unexecuted() {
    let pool = Arc::new(Pool::new());
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open_with(
            dir.path(),
            Capacity {
                objects: 1,
                transactions: 1,
            },
        )
        .unwrap(),
    );
    // How many transactions are written: each below is executed.
    let written = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..4)
        .map(|r| {
            let (store, written) = (store.clone(), written.clone());
            std::thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(r);
                loop {
                    let below = written.load(Ordering::Acquire);
                    if below == TXS {
                        return;
                    }
                    // Mostly the newest, whose answers are in flux.
                    let n = below.saturating_sub(rng.gen_range(0..4));
                    let found = store.executed_effects(&digest(1, n)).unwrap();
                    if n < below {
                        assert_eq!(found, Some(digest(2, n)), "{n} unexecuted");
                    }
                }
            })
        })
        .collect();
    let mut pending = Vec::new();
    for n in 0..TXS {
        let tx = Tx {
            digest: digest(1, n),
            effects_digest: digest(2, n),
            pool: 0,
            outputs: vec![],
        };
        store.write(tx.commit(&pool));
        written.store(n + 1, Ordering::Release);
        pending.push(tx.digest);
        if n % 2 == 1 {
            store.commit_checkpoint(n / 2, &pending, vec![]);
            pending.clear();
        }
    }
    for reader in readers {
        reader.join().unwrap();
    }
}
