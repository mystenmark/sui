// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Scheduling a commit: the order of its transactions, and the ordinals of
//! their shared-object accesses.

use containers::Bump;
use messages::Message;
use messages::base::ObjectId;
use messages::transaction::SharedObjectMutability as Mutability;
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::transaction::{
    CallArg, ObjectArg, ProgrammableTransaction, SharedObjectMutability, Transaction,
    TransactionData, TransactionKind,
};
use validator::checks::{self, VerifiedTransaction};
use validator::consensus::sequence::{
    Prologue, Schedule, Scheduled, Sequencer, SharedAccess, SharedOrdinal,
};

/// A shared input: an object id, its initial shared version, and how it is
/// accessed.
type Input = (u16, u64, SharedObjectMutability);

const W: SharedObjectMutability = SharedObjectMutability::Mutable;
const R: SharedObjectMutability = SharedObjectMutability::Immutable;
const N: SharedObjectMutability = SharedObjectMutability::NonExclusiveWrite;

/// A transaction at `price` with `inputs`, taken from consensus.
fn transaction(price: u64, inputs: &[Input]) -> VerifiedTransaction {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let pt = ProgrammableTransaction {
        inputs: inputs
            .iter()
            .map(|(id, version, mutability)| {
                CallArg::Object(ObjectArg::SharedObject {
                    id: ObjectID::from_single_byte(*id as u8),
                    initial_shared_version: SequenceNumber::from_u64(*version),
                    mutability: *mutability,
                })
            })
            .collect(),
        commands: vec![],
    };
    let data = TransactionData::new(
        TransactionKind::ProgrammableTransaction(pt),
        sender,
        random_object_ref(),
        1_000_000,
        price,
    );
    let bytes = bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap();
    checks::sequenced_by_consensus(Message::parse(bytes).map_err(|(e, _)| e).unwrap())
}

fn prologue(round: u32) -> Prologue {
    Prologue {
        epoch: 0,
        round,
        commit_index: round,
        timestamp_ms: 1000 * u64::from(round),
        commit_digest: [round as u8; 32],
    }
}

fn config() -> ProtocolConfig {
    ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown)
}

fn schedule(round: u32, transactions: Vec<VerifiedTransaction>) -> Schedule<'static> {
    order_and_schedule(&mut Sequencer::new(), round, transactions)
}

/// In an arena of its own, which lives as long as the test.
fn order_and_schedule(
    sequencer: &mut Sequencer,
    round: u32,
    transactions: Vec<VerifiedTransaction>,
) -> Schedule<'static> {
    let bump: &'static Bump = Box::leak(Box::default());
    let mut arena = containers::Vec::with_capacity_in(transactions.len(), bump);
    arena.extend(transactions);
    let ordered = sequencer.order(&config(), arena, bump);
    let mut scheduled = containers::Vec::with_capacity_in(ordered.len(), bump);
    scheduled.extend(ordered.into_iter().map(Scheduled::User));
    sequencer.schedule(prologue(round), None, None, scheduled, bump)
}

/// The generation each access reads, by transaction, prologue first.
fn generations(schedule: &Schedule<'_>) -> Vec<Vec<u32>> {
    (0..schedule.len())
        .map(|i| {
            schedule
                .accesses(i)
                .iter()
                .map(|a| a.ordinal.writes_before)
                .collect()
        })
        .collect()
}

/// The user transactions' digests, in schedule order.
fn order(schedule: &Schedule<'_>) -> Vec<[u8; 32]> {
    schedule
        .transactions
        .iter()
        .filter_map(|t| match t {
            Scheduled::User(t) => Some(t.get().0.digest().bytes),
            Scheduled::Prologue(_)
            | Scheduled::AuthenticatorStateUpdate(_)
            | Scheduled::RandomnessStateUpdate(_)
            | Scheduled::Cancelled(..) => None,
        })
        .collect()
}

#[test]
fn the_prologue_writes_the_clock_first() {
    let reader = transaction(1000, &[(6, 1, R)]);
    let s = schedule(7, vec![reader]);
    assert!(matches!(s.transactions[0], Scheduled::Prologue(p) if p == prologue(7)));
    assert_eq!(
        s.accesses(0),
        [SharedAccess {
            id: ObjectId::from_u16(6),
            initial_shared_version: 1,
            mutability: Mutability::Mutable,
            ordinal: SharedOrdinal {
                round: 7,
                writes_before: 0
            },
            base: None,
        }]
    );
    // A reader of the clock reads what the prologue wrote.
    assert_eq!(
        s.accesses(1)[0].ordinal,
        SharedOrdinal {
            round: 7,
            writes_before: 1
        }
    );
}

#[test]
fn only_exclusive_writes_start_a_generation() {
    let s = schedule(
        3,
        vec![
            transaction(1000, &[(9, 2, W)]),
            transaction(1000, &[(9, 2, R)]),
            transaction(1000, &[(9, 2, N)]),
            transaction(1000, &[(9, 2, W)]),
            transaction(1000, &[(9, 2, R)]),
            transaction(1000, &[]),
        ],
    );
    assert_eq!(
        generations(&s),
        [vec![0], vec![0], vec![1], vec![1], vec![1], vec![2], vec![]]
    );
    assert_eq!(s.accesses(3)[0].mutability, Mutability::NonExclusiveWrite);
}

#[test]
fn a_transaction_reads_before_its_own_writes_and_writes_a_stream_once() {
    let s = schedule(
        3,
        vec![
            transaction(1000, &[(9, 2, W), (9, 2, W), (9, 2, R)]),
            transaction(1000, &[(9, 2, R)]),
        ],
    );
    assert_eq!(generations(&s), [vec![0], vec![0, 0, 0], vec![1]]);
}

#[test]
fn streams_are_an_id_and_an_initial_version() {
    let s = schedule(
        3,
        vec![
            transaction(1000, &[(9, 2, W)]),
            transaction(1000, &[(9, 5, R), (9, 2, R), (10, 2, R)]),
        ],
    );
    assert_eq!(generations(&s), [vec![0], vec![0], vec![0, 1, 0]]);
}

#[test]
fn higher_gas_prices_go_first_and_ties_keep_commit_order() {
    let transactions = [
        transaction(1000, &[(9, 2, W)]),
        transaction(3000, &[(9, 2, W)]),
        transaction(2000, &[(9, 2, W)]),
        transaction(3000, &[(9, 2, W)]),
    ];
    let digests: Vec<_> = transactions
        .iter()
        .map(|t| t.get().0.digest().bytes)
        .collect();
    let s = schedule(3, transactions.into_iter().collect());
    assert_eq!(order(&s), [digests[1], digests[3], digests[2], digests[0]]);
    // Ordinals follow the schedule, not the commit.
    assert_eq!(
        generations(&s),
        [vec![0], vec![0], vec![1], vec![2], vec![3]]
    );
}

#[test]
fn transactions_using_randomness_go_last() {
    let random = transaction(9000, &[(8, 4, R)]);
    let plain = transaction(1000, &[]);
    let digests = [random.get().0.digest().bytes, plain.get().0.digest().bytes];
    let s = schedule(3, vec![random, plain]);
    assert_eq!(order(&s), [digests[1], digests[0]]);
}

#[test]
fn each_commit_starts_over() {
    let mut sequencer = Sequencer::new();
    let first = order_and_schedule(
        &mut sequencer,
        3,
        vec![
            transaction(1000, &[(9, 2, W)]),
            transaction(1000, &[(9, 2, W)]),
        ],
    );
    assert_eq!(generations(&first), [vec![0], vec![0], vec![1]]);
    let second = order_and_schedule(&mut sequencer, 5, vec![transaction(1000, &[(9, 2, W)])]);
    assert_eq!(
        second.accesses(1)[0].ordinal,
        SharedOrdinal {
            round: 5,
            writes_before: 0
        }
    );
    assert!(
        first.accesses(2)[0].ordinal < second.accesses(1)[0].ordinal,
        "ordinals order accesses across commits"
    );
}

/// A xorshift generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// The ordinals name the versions the reference assigns. The reference's
/// rule (`assign_versions_for_certificate`): each shared input gets its
/// stream's next version, the transaction's version is one past the highest
/// of its input versions (owned ones included), and only exclusive writes
/// move a stream's next version to it. Executed in schedule order, the
/// stream's generation `k` in a commit is the version its `k`th exclusive
/// writer there produced, or the stream's version before the commit for
/// `k = 0`.
#[test]
fn ordinals_name_the_versions_the_reference_assigns() {
    use std::collections::HashMap;
    let mutabilities = [W, R, N];
    for seed in 1..=50u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut sequencer = Sequencer::new();
        // The reference's next version of each stream, across commits.
        let mut next: HashMap<(ObjectId, u64), u64> = HashMap::new();
        for round in 1..=4u32 {
            let transactions: Vec<_> = (0..rng.below(12))
                .map(|_| {
                    let inputs: Vec<Input> = (0..rng.below(4))
                        .map(|_| {
                            let id = 9 + rng.below(3) as u16;
                            (id, 1 + rng.below(2), mutabilities[rng.below(3) as usize])
                        })
                        .collect();
                    transaction(1000 * (1 + rng.below(3)), &inputs)
                })
                .collect();
            let s = order_and_schedule(&mut sequencer, round, transactions);
            // Each stream's versions by generation in this commit.
            let mut generations: HashMap<(ObjectId, u64), Vec<u64>> = HashMap::new();
            for i in 0..s.len() {
                let accesses = s.accesses(i);
                let mut lamport = 0;
                for a in accesses {
                    let stream = (a.id, a.initial_shared_version);
                    let reference = *next.entry(stream).or_insert(a.initial_shared_version);
                    let ours = generations.entry(stream).or_insert_with(|| vec![reference]);
                    assert_eq!(a.ordinal.round, round);
                    assert_eq!(
                        ours[a.ordinal.writes_before as usize], reference,
                        "seed {seed} round {round} transaction {i}"
                    );
                    lamport = lamport.max(reference);
                }
                // An owned input, as most transactions have.
                let lamport = lamport.max(rng.below(20)) + 1;
                let mut written = Vec::new();
                for a in accesses {
                    let stream = (a.id, a.initial_shared_version);
                    if a.mutability == Mutability::Mutable && !written.contains(&stream) {
                        written.push(stream);
                        next.insert(stream, lamport);
                        generations.get_mut(&stream).unwrap().push(lamport);
                    }
                }
            }
        }
    }
}
