// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Scheduling commits: ordering by gas price and giving shared accesses
//! their ordinals. Commits of 100 and 1000 transactions, each with zero to
//! three shared inputs drawn from a few hot objects or from many, written
//! or read at random. Time and allocations per transaction, the
//! transactions parsed again outside the measurement. Run with
//! `cargo bench -p validator --bench schedule`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use messages::Message;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::transaction::{
    CallArg, ObjectArg, ProgrammableTransaction, SharedObjectMutability, Transaction,
    TransactionData, TransactionKind,
};
use validator::checks::{self, VerifiedTransaction};
use validator::consensus::sequence::{Prologue, Sequencer};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: defers to `System` and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// `count` signed transactions, as `Transaction` BCS, with shared inputs
/// among `objects` objects.
fn transactions(count: usize, objects: u64, rng: &mut Rng) -> Vec<Vec<u8>> {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    (0..count)
        .map(|_| {
            let inputs = (0..rng.below(4))
                .map(|_| {
                    let mut id = [0u8; 32];
                    id[..8].copy_from_slice(&rng.below(objects).to_le_bytes());
                    CallArg::Object(ObjectArg::SharedObject {
                        id: ObjectID::new(id),
                        initial_shared_version: SequenceNumber::from_u64(1),
                        mutability: if rng.below(2) == 0 {
                            SharedObjectMutability::Mutable
                        } else {
                            SharedObjectMutability::Immutable
                        },
                    })
                })
                .collect();
            let pt = ProgrammableTransaction {
                inputs,
                commands: vec![],
            };
            let data = TransactionData::new(
                TransactionKind::ProgrammableTransaction(pt),
                sender,
                random_object_ref(),
                1_000_000,
                1000 * (1 + rng.below(5)),
            );
            bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap()
        })
        .collect()
}

fn parse(bytes: &[Vec<u8>]) -> Vec<VerifiedTransaction> {
    bytes
        .iter()
        .map(|b| {
            checks::sequenced_by_consensus(Message::parse(b.clone()).map_err(|(e, _)| e).unwrap())
        })
        .collect()
}

fn main() {
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    println!("commit  objects  ns/tx  allocs/commit");
    for commit_size in [100, 1000] {
        for objects in [10, 100_000] {
            let commit = transactions(commit_size, objects, &mut rng);
            let mut sequencer = Sequencer::new();
            let mut elapsed = Duration::ZERO;
            let mut allocations = 0;
            let rounds = 200;
            for round in 0..=rounds {
                let transactions = parse(&commit);
                let prologue = Prologue {
                    epoch: 0,
                    round,
                    commit_index: round,
                    timestamp_ms: 0,
                    commit_digest: [0; 32],
                };
                let before = ALLOCATIONS.load(Ordering::Relaxed);
                let start = Instant::now();
                let schedule = sequencer.schedule(&config, prologue, transactions);
                let took = start.elapsed();
                let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
                assert_eq!(schedule.len(), commit_size + 1);
                // The first round sizes the tables.
                if round > 0 {
                    elapsed += took;
                    allocations += allocated;
                }
            }
            println!(
                "{commit_size:<6}  {objects:<7}  {:>5.1}  {:>13}",
                elapsed.as_nanos() as f64 / (f64::from(rounds) * commit_size as f64),
                allocations / rounds as usize,
            );
        }
    }
}
