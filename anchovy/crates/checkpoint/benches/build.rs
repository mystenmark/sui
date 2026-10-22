// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Building checkpoints of 100 and 1000 transactions, each signed by one
//! sender and writing its gas coin, a shared object and a new object, in
//! two chunks: the contents (chunk table, stored transactions, effects
//! parsed for gas and artifacts, contents BCS and digest), then the
//! summary, then the signature over it. Time and allocations per
//! transaction for contents, per checkpoint for the summary and signature.
//! Run with `cargo bench -p checkpoint --bench build`.
//!
//! On an M-series Mac, with other builds running (runs varied by up to
//! half):
//!
//! ```text
//! txs   contents ns/tx  allocs/tx  read ns/tx  allocs/tx  summary ns  allocs  sign ns
//! 100           2201.2        8.2       617.3        8.0       732.9       3  135421.2
//! 1000          2917.5        8.0       942.4        8.0      1859.2       3  323433.3
//! ```
//!
//! "read" is reading and parsing the stored transactions alone, for their
//! signatures: every allocation of the contents and about a third of the
//! time. The rest is parsing each effects (which hashes it again), the
//! artifacts' Merkle tree and the contents digest. Signing (BLS) dominates
//! a checkpoint's summary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use checkpoint::{ContentsBuilder, ProtocolKey, summary};
use messages::base::Digest;
use messages::fast::Bump;
use protocol_config::{Chain, ProtocolVersion};
use store::{Commit, Executed, Store};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::effects::{EffectsObjectChange, TransactionEffects};
use sui_types::execution_status::ExecutionStatus;
use sui_types::gas::GasCostSummary;
use sui_types::object::{Object, Owner};
use sui_types::transaction::{
    ProgrammableTransaction, Transaction, TransactionData, TransactionKind,
};
use validator::epoch::EpochParams;
use validator::pipeline::items::{ExecutedEffects, PendingCheckpoint, SettledChunk};
use validator::pipeline::tables::{BuiltCheckpoint, ChunkTable};

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

/// `count` executed transactions, committed to `store`, as the chunk table
/// keeps their effects.
fn executed(count: usize, store: &Store) -> Vec<ExecutedEffects> {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let shared = ObjectID::random();
    (0..count)
        .map(|i| {
            let data = TransactionData::new(
                TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
                    inputs: vec![],
                    commands: vec![],
                }),
                sender,
                random_object_ref(),
                1_000_000,
                1000,
            );
            let transaction = Transaction::from_data_and_signer(data, vec![&key]);
            let digest = *transaction.digest();
            let lamport = SequenceNumber::from_u64(100 + i as u64);
            let owned = Owner::AddressOwner(sender);
            let gas = Object::with_id_owner_version_for_testing(
                ObjectID::random(),
                lamport,
                owned.clone(),
            );
            let created = Object::with_id_owner_version_for_testing(
                ObjectID::random(),
                lamport,
                owned.clone(),
            );
            let counter = Object::with_id_owner_version_for_testing(shared, lamport, owned.clone());
            let before = ((SequenceNumber::from_u64(1), gas.digest()), owned.clone());
            let changes = BTreeMap::from([
                (
                    gas.id(),
                    EffectsObjectChange::new(Some(before.clone()), Some(&gas), false, false),
                ),
                (
                    created.id(),
                    EffectsObjectChange::new(None, Some(&created), true, false),
                ),
                (
                    shared,
                    EffectsObjectChange::new(Some(before), Some(&counter), false, false),
                ),
            ]);
            let effects = TransactionEffects::new_from_execution_v2(
                ExecutionStatus::Success,
                0,
                GasCostSummary::new(1000, 2000, 1500, 20),
                vec![],
                digest,
                lamport,
                changes,
                Some(gas.id()),
                None,
                vec![],
            );
            let effects_bytes = bcs::to_bytes(&effects).unwrap();
            let effects_digest = Digest::of("TransactionEffects", &effects_bytes);
            store
                .write_through(Commit {
                    executed: Some(Executed {
                        digest: Digest::new(digest.into_inner()),
                        transaction: bcs::to_bytes(transaction.data()).unwrap(),
                        effects_digest,
                        effects: effects_bytes.clone(),
                        events: None,
                    }),
                    ..Commit::default()
                })
                .unwrap();
            ExecutedEffects {
                digest: digest.into_inner(),
                effects_digest: effects_digest.bytes,
                effects: Arc::from(effects_bytes),
            }
        })
        .collect()
}

fn per(elapsed: Duration, n: usize) -> f64 {
    elapsed.as_nanos() as f64 / n as f64
}

fn main() {
    let epoch = Arc::new(EpochParams::new(
        Chain::Mainnet,
        ProtocolVersion::MAX.as_u64(),
        1,
        Digest::ZERO,
        1000,
        4,
    ));
    let mut private = [0u8; 32];
    private[31] = 7;
    let key = ProtocolKey::from_private_bytes(&private).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());

    println!("txs   contents ns/tx  allocs/tx  read ns/tx  allocs/tx  summary ns  allocs  sign ns");
    for size in [100, 1000] {
        let mut effects = executed(size, &store);
        let digests: Vec<Digest> = effects.iter().map(|e| Digest::new(e.digest)).collect();
        let rest = effects.split_off(size / 2);
        let chunks = Arc::new(ChunkTable::new());
        for (height, effects) in [(0, effects), (1, rest)] {
            chunks.insert(SettledChunk {
                height,
                checkpoint_seq: 1,
                effects,
            });
        }
        let (hasher, _hasher_inbox) = workqueue::queue(1);
        let (sequence, _sequence_inbox) = workqueue::queue(1);
        let mut builder =
            ContentsBuilder::new(epoch.clone(), chunks, store.clone(), hasher, sequence);
        let previous = BuiltCheckpoint {
            seq: 0,
            summary: 1u64.to_le_bytes().to_vec(),
            digest: [1; 32],
            contents: Vec::new(),
            network_total_transactions: 10,
            epoch_rolling_gas: [1, 2, 3, 4],
            last_height: 0,
            timestamp_ms: 0,
            root_state: None,
        };
        // As a worker's arena: reset after each item, never freed.
        let mut bump = Bump::with_capacity(1 << 20);

        let rounds = 100;
        let (mut contents_time, mut contents_allocs) = (Duration::ZERO, 0);
        let (mut summary_time, mut summary_allocs) = (Duration::ZERO, 0);
        let mut sign_time = Duration::ZERO;
        for round in 0..=rounds {
            let pending = PendingCheckpoint {
                seq: 1,
                heights: vec![0, 1],
                timestamp_ms: 0,
                close: None,
            };
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            let (built, built_effects) = builder.build(pending, &mut bump);
            let took = start.elapsed();
            let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
            assert_eq!(built_effects.len(), size);

            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            let summary = summary::build(&built, Some(&previous), &epoch, &bump);
            let summary_took = start.elapsed();
            let summary_allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;

            let start = Instant::now();
            std::hint::black_box(summary::sign(&key, &summary.bytes, 1, &bump));
            let sign_took = start.elapsed();

            // The first round warms the arenas and the store's caches.
            if round > 0 {
                contents_time += took;
                contents_allocs += allocated;
                summary_time += summary_took;
                summary_allocs += summary_allocated;
                sign_time += sign_took;
            }
            bump.reset();
        }
        // The part of the contents that is reading the transactions.
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for _ in 0..rounds {
            for digest in &digests {
                std::hint::black_box(store.transaction(digest).unwrap().unwrap());
            }
        }
        let read_time = start.elapsed();
        let read_allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
        println!(
            "{size:<5} {:>14.1}  {:>9.1}  {:>10.1}  {:>9.1}  {:>10.1}  {:>6}  {:>7.1}",
            per(contents_time, rounds * size),
            contents_allocs as f64 / (rounds * size) as f64,
            per(read_time, rounds * size),
            read_allocs as f64 / (rounds * size) as f64,
            per(summary_time, rounds),
            summary_allocs / rounds,
            per(sign_time, rounds),
        );
    }
}
