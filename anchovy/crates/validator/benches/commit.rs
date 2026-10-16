// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Commits through `CommitStage`: deduplication, owned-object locks,
//! scheduling, the prologue, chunking into checkpoints, and emitting chunks,
//! settlement work and checkpoint waits. Commits of 100 and 1000
//! transactions, each paying with its own gas coin (the one owned input it
//! locks) and with zero to three distinct shared inputs drawn from a few hot
//! objects or from many. Every commit's transactions are new, as
//! deduplication would drop repeats. Time and allocations per transaction;
//! parsing the transactions and draining the outputs are outside the
//! measurement. Run with `cargo bench -p validator --bench commit`.
//!
//! On an M-series laptop, under load from other builds, runs interleaved;
//! "before" is the handler without owned-object locks:
//!
//! ```text
//!                  before            after
//! commit  objects  ns/tx  allocs/tx  ns/tx  allocs/tx
//! 100     10         279        1.2    366        1.2
//! 100     100000     285        1.2    314        1.2
//! 1000    10         264        0.8    336        0.8
//! 1000    100000     253        0.8    326        0.8
//! ```
//!
//! Locking costs about 60 ns per transaction: one probe of the epoch's lock
//! table, which grows by every owned reference locked (51,000 here). Most
//! allocations are each entry's own vector of shared accesses; the rest are
//! the per-chunk vectors and the prologue.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use consensus::BlockRef;
use messages::Message;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::transaction::{
    CallArg, ObjectArg, ProgrammableTransaction, SharedObjectMutability, Transaction,
    TransactionData, TransactionKind,
};
use validator::checks;
use validator::consensus::commit::{CommitStage, EpochStart};
use validator::consensus::decode::{CommittedTransaction, DecodedCommit};
use validator::epoch::EpochParams;
use validator::epoch_store::{ConsensusOutput, EpochStore};
use workqueue::Processor;

struct Counting;

/// The one writer of a fresh epoch store's consensus output, and the
/// directory the store is in.
fn consensus_output() -> (ConsensusOutput, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output) = EpochStore::open(dir.path(), 0);
    (output, dir)
}

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
            // Distinct objects: a transaction naming one twice has invalid
            // inputs, and is dropped.
            let mut ids: Vec<u64> = (0..rng.below(4)).map(|_| rng.below(objects)).collect();
            ids.sort_unstable();
            ids.dedup();
            let inputs = ids
                .into_iter()
                .map(|object| {
                    let mut id = [0u8; 32];
                    id[..8].copy_from_slice(&object.to_le_bytes());
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

fn commit(index: u32, transactions: &[Vec<u8>]) -> DecodedCommit {
    let block = BlockRef {
        round: index,
        author: 0,
        digest: [0; 32],
    };
    DecodedCommit {
        commit_index: index,
        leader: block,
        timestamp_ms: 250 * u64::from(index),
        digest: [index as u8; 32],
        transactions: transactions
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                CommittedTransaction::new(
                    block,
                    i as u16,
                    checks::sequenced_by_consensus(
                        Message::parse(bytes.clone()).map_err(|(e, _)| e).unwrap(),
                    ),
                    &[],
                )
            })
            .collect(),
        rejected: Vec::new(),
        cache_hits: 0,
        cache_misses: 0,
    }
}

fn main() {
    let epoch = Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        messages::base::Digest::new([0; 32]),
        1000,
        4,
    ));
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let rounds: u32 = 50;
    println!("commit  objects  ns/tx  allocs/tx");
    for commit_size in [100, 1000] {
        for objects in [10, 100_000] {
            let (chunks, chunk_inbox) = workqueue::queue(1024);
            let (settlements, settlement_inbox) = workqueue::queue(1024);
            let (checkpoints, checkpoint_inbox) = workqueue::queue(1024);
            let start = EpochStart {
                previous_checkpoint: 0,
                previous_height: 0,
                previous_timestamp_ms: 0,
                accumulator_root_initial_shared_version: 1,
                accumulator_root_version: 1,
                epoch_start_timestamp_ms: 0,
                epoch_duration_ms: u64::MAX,
            };
            let (output, _dir) = consensus_output();
            let mut stage = CommitStage::new(
                epoch.clone(),
                start,
                output,
                chunks,
                settlements,
                checkpoints,
                Arc::new(validator::consensus::status::ConsensusTxStatusCache::new()),
            );
            let mut elapsed = Duration::ZERO;
            let mut allocations = 0;
            for index in 1..=rounds + 1 {
                let request = commit(index, &transactions(commit_size, objects, &mut rng));
                let before = ALLOCATIONS.load(Ordering::Relaxed);
                let begin = Instant::now();
                stage.process(request).unwrap();
                let took = begin.elapsed();
                let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
                let mut emitted = 0;
                while let Some(chunk) = chunk_inbox.try_pop() {
                    emitted += chunk.entries.len();
                }
                while settlement_inbox.try_pop().is_some() {}
                while checkpoint_inbox.try_pop().is_some() {}
                assert_eq!(emitted, commit_size + 1);
                // The first commit sizes the tables.
                if index > 1 {
                    elapsed += took;
                    allocations += allocated;
                }
            }
            let transactions = f64::from(rounds) * commit_size as f64;
            println!(
                "{commit_size:<6}  {objects:<7}  {:>5.0}  {:>9.1}",
                elapsed.as_nanos() as f64 / transactions,
                allocations as f64 / transactions,
            );
        }
    }
}
