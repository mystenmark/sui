// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Hashing checkpoints of 100 and 1000 transactions' effects, each with one
//! to eight object changes of every kind, through `StateHasher` (parse,
//! ECMH, fold). Time per transaction and per checkpoint, and allocations per
//! checkpoint. Run with `cargo bench -p state-hasher --bench hash`.
//!
//! On an Apple M-series laptop (about 4.5 changes per transaction):
//!
//! ```text
//! checkpoint  us/tx  ms/checkpoint  allocs/checkpoint
//! 100          21.1           2.11                  0
//! 1000         19.9          19.91                  0
//! ```
//!
//! Nearly all of it is ECMH's hash to the Ristretto group, about 4us per
//! inserted or removed digest; the reference spends the same, serially.
//! Allocations are only the maps' occasional B-tree nodes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use state_hasher::{StateHash, StateHasher, StateHashes};
use validator::pipeline::items::CheckpointEffects;
use workqueue::Processor;

#[path = "../tests/support/effects.rs"]
mod effects;

use effects::{Rng, checkpoint};

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

fn main() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    println!("checkpoint  us/tx  ms/checkpoint  allocs/checkpoint");
    for size in [100, 1000] {
        let effects = checkpoint(&mut rng, size);
        let hashes = Arc::new(StateHashes::new());
        let mut processor = StateHasher::new(hashes.clone(), 0, StateHash::default());
        let rounds: u32 = if size == 100 { 100 } else { 20 };
        let mut elapsed = Duration::ZERO;
        let mut allocations = 0;
        for seq in 0..=u64::from(rounds) {
            let item = CheckpointEffects {
                seq,
                effects: effects.clone(),
            };
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            processor.process(item);
            let took = start.elapsed();
            let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
            assert!(hashes.root(seq).is_some());
            // The first round warms the parse arena.
            if seq > 0 {
                elapsed += took;
                allocations += allocated;
            }
        }
        let per_checkpoint = elapsed.as_secs_f64() / f64::from(rounds);
        println!(
            "{size:<10}  {:>5.1}  {:>13.2}  {:>17}",
            per_checkpoint * 1e6 / size as f64,
            per_checkpoint * 1e3,
            allocations / rounds as usize,
        );
    }
}
