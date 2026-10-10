// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Voting on blocks of signed transfers against a funded genesis, each
//! transfer from its own sender, paying with its own gas coin: time and allocations per
//! transaction, with signatures not yet verified (a peer's transactions) and
//! verified before (the signature cache warm). Run with
//! `cargo bench -p validator --bench vote`; `TXS` sets the transaction count,
//! `PASSES` how many times each is voted on, and `MODE` (`cold` or `warm`)
//! measures only one, for profiling.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use consensus::{Block, BlockRef};
use messages::base::{Digest, ObjectId};
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::object::Object;
use sui_types::transaction::{Transaction, TransactionData};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::vote::BlockVoter;
use validator::epoch::EpochState;

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

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;
const BLOCK_SIZE: usize = 100;

/// A `UserTransactionV2` of `transaction`, claiming its one signature and no
/// alias.
fn user_transaction(transaction: &[u8]) -> Vec<u8> {
    let mut bytes = vec![7; 8];
    bytes.push(12);
    bytes.extend_from_slice(transaction);
    bytes.extend_from_slice(&[1, 2, 1, 0, 0]);
    bytes
}

/// A transfer from each sender, paying with the gas coin genesis gave it, as
/// `UserTransactionV2`s.
fn signed_transfers(
    store: &store::Store,
    senders: &[(SuiAddress, AccountKeyPair)],
) -> Vec<Vec<u8>> {
    (0..senders.len())
        .map(|i| {
            let id = ObjectID::derive_id(TransactionDigest::genesis_marker(), i as u64);
            let coin = store
                .live_object(&ObjectId(id.into_bytes()))
                .unwrap()
                .unwrap();
            let gas = bcs::from_bytes::<Object>(coin.wire_bytes())
                .unwrap()
                .compute_object_reference();
            let (sender, key) = &senders[i];
            let data = TransactionData::new_transfer_sui(
                SuiAddress::random_for_testing_only(),
                *sender,
                Some(SUI),
                gas,
                BUDGET,
                RGP,
            );
            user_transaction(
                &bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![key])).unwrap(),
            )
        })
        .collect()
}

fn main() {
    let count: usize = std::env::var("TXS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(2_000);
    let senders: Vec<(SuiAddress, AccountKeyPair)> = (0..count)
        .map(|_| get_key_pair::<AccountKeyPair>())
        .collect();
    let epoch = Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        Digest::new([0; 32]),
        RGP,
        4,
        [],
    ));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let funded: Vec<_> = senders
        .iter()
        .map(|(sender, _)| (sender.to_inner(), 10 * SUI))
        .collect();
    assert!(execution::genesis::init(&epoch.execution, &store, &funded).unwrap());

    let transactions = signed_transfers(&store, &senders);
    let blocks: Vec<Block> = transactions
        .chunks(BLOCK_SIZE)
        .enumerate()
        .map(|(round, transactions)| Block {
            reference: BlockRef {
                round: round as u32 + 1,
                author: 1,
                digest: [round as u8; 32],
            },
            transactions: transactions.to_vec(),
        })
        .collect();

    let passes: usize = std::env::var("PASSES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    let mode = std::env::var("MODE").ok();
    let cache = Arc::new(ConsensusTxCache::new());
    let mut warm_voter = BlockVoter::new(store.clone(), cache.clone());
    println!("signatures  txs    vote µs/tx  allocs/tx");
    for label in ["cold", "warm"] {
        if mode.as_deref().is_some_and(|m| m != label) {
            continue;
        }
        if label == "warm" {
            // Verify every signature once, unmeasured.
            for block in &blocks {
                warm_voter.vote(&epoch, block.clone()).unwrap();
                cache.take(&block.reference);
            }
        }
        let mut elapsed = std::time::Duration::ZERO;
        let mut allocations = 0;
        for _ in 0..passes {
            // A fresh voter has verified no signatures.
            let mut cold_voter = BlockVoter::new(store.clone(), cache.clone());
            let voter = if label == "cold" {
                &mut cold_voter
            } else {
                &mut warm_voter
            };
            for block in &blocks {
                let block = block.clone();
                let before = ALLOCATIONS.load(Ordering::Relaxed);
                let start = Instant::now();
                let verdict = voter.vote(&epoch, block);
                elapsed += start.elapsed();
                allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
                assert_eq!(verdict, Ok(vec![]));
            }
            // The cache's entries are freed outside the measurement.
            for block in &blocks {
                cache.take(&block.reference);
            }
        }
        let votes = count * passes;
        println!(
            "{label:<10}  {count:<5}  {:>11.1}  {:>9}",
            elapsed.as_secs_f64() * 1e6 / votes as f64,
            allocations / votes
        );
    }
}
