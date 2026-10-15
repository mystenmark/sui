// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Aggregating checkpoint signatures: committees of 4 and 100, every member
//! signing each checkpoint, all of a checkpoint's signatures in one batch.
//! Those past the quorum are dropped as the checkpoint is certified. Time
//! and allocations per checkpoint and per signature, the signatures made
//! outside the measurement. Run with
//! `cargo bench -p aggregator --bench aggregate`.
//!
//! On an Apple M-series laptop:
//!
//! ```text
//! committee  us/checkpoint  us/signature  allocs/checkpoint
//! 4                  575.4         143.8                 16
//! 100               1172.3          11.7                 30
//! ```
//!
//! Nearly all of it is the one aggregate verification at quorum; the rest
//! is aggregating the quorum's signatures and public keys.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aggregator::{CheckpointAggregator, Committee};
use sui_types::committee::Committee as SuiCommittee;
use sui_types::crypto::KeypairTraits;
use sui_types::digests::CheckpointContentsDigest;
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CheckpointSignatureMessage, CheckpointSummary, SignedCheckpointSummary,
};
use validator::pipeline::items::CheckpointSignature;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use workqueue::Processor;

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

fn summary(seq: u64) -> CheckpointSummary {
    CheckpointSummary {
        epoch: 0,
        sequence_number: seq,
        network_total_transactions: 1000 * seq,
        content_digest: CheckpointContentsDigest::new([seq as u8; 32]),
        previous_digest: None,
        epoch_rolling_gas_cost_summary: GasCostSummary::new(1, 2, 3, 4),
        timestamp_ms: 1_700_000_000_000 + seq,
        checkpoint_commitments: vec![],
        end_of_epoch_data: None,
        version_specific_data: vec![0; 40],
    }
}

fn main() {
    println!("committee  us/checkpoint  us/signature  allocs/checkpoint");
    for (size, rounds) in [(4, 200), (100, 20)] {
        let (sui, keys) = SuiCommittee::new_simple_test_committee_of_size(size);
        let committee = Arc::new(
            Committee::new(
                0,
                sui.voting_rights
                    .iter()
                    .map(|(name, stake)| (name.0, *stake)),
            )
            .unwrap(),
        );
        let checkpoints = Arc::new(CheckpointStore::new());
        let (state_sync, certified) = workqueue::queue(rounds + 1);
        let mut aggregator =
            CheckpointAggregator::new(committee, checkpoints.clone(), 0, state_sync);

        let batches: Vec<Vec<CheckpointSignature>> = (0..=rounds as u64)
            .map(|seq| {
                let summary = summary(seq);
                checkpoints.insert_built(BuiltCheckpoint {
                    seq,
                    summary: bcs::to_bytes(&summary).unwrap(),
                    digest: summary.digest().into_inner(),
                    contents: vec![],
                    network_total_transactions: summary.network_total_transactions,
                    epoch_rolling_gas: [1, 2, 3, 4],
                    last_height: 0,
                    timestamp_ms: 0,
                });
                keys.iter()
                    .map(|key| {
                        let signed = SignedCheckpointSummary::new(
                            0,
                            summary.clone(),
                            key,
                            key.public().into(),
                        );
                        let message = CheckpointSignatureMessage { summary: signed };
                        CheckpointSignature {
                            seq,
                            message: bcs::to_bytes(&message).unwrap().into(),
                        }
                    })
                    .collect()
            })
            .collect();

        let mut elapsed = Duration::ZERO;
        let mut allocations = 0;
        for (round, batch) in batches.into_iter().enumerate() {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            aggregator.process(batch);
            let took = start.elapsed();
            let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
            assert_eq!(certified.try_pop().unwrap().seq, round as u64);
            // The first round warms up.
            if round > 0 {
                elapsed += took;
                allocations += allocated;
            }
        }
        let per_checkpoint = elapsed.as_secs_f64() * 1e6 / rounds as f64;
        println!(
            "{size:<9}  {per_checkpoint:>13.1}  {:>12.1}  {:>17}",
            per_checkpoint / size as f64,
            allocations / rounds,
        );
    }
}
