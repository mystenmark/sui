// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Executing through `Executor`: independent SUI transfers, and a chain of
//! writes to one shared coin, each at the generation before it, in batches
//! of 100 as the input waiter hands them out. Time and allocations per
//! transaction, for `process` alone (executing with sui's executor,
//! committing to the store, recording generations, notifying); the
//! transactions are signed beforehand. Run with
//! `cargo bench -p exec-pool --bench execute`.
//!
//! On an M-series Mac, 1000 transactions each:
//! - transfers: about 91 µs and 940 allocations per transaction;
//! - shared increments: about 149 µs and 1,930 allocations per
//!   transaction.
//!
//! Nearly all of it is sui's executor and the store, which the executor
//! calls once per transaction.

#[path = "../tests/common/mod.rs"]
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::Node;
use validator::pipeline::items::Executable;
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

const TRANSACTIONS: usize = 1000;
const BATCH: usize = 100;

/// Runs `executables` through `node`'s executor in batches, timing and
/// counting only `process`; the notifications are drained between batches.
fn run(name: &str, node: &mut Node, executables: Vec<Executable>) {
    let count = executables.len();
    let mut elapsed = Duration::ZERO;
    let mut allocations = 0;
    let mut executables = executables.into_iter();
    loop {
        let batch: Vec<Executable> = executables.by_ref().take(BATCH).collect();
        if batch.is_empty() {
            break;
        }
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        node.executor.process(batch);
        elapsed += start.elapsed();
        allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
        while node.notified_inputs.try_pop().is_some() {}
        while node.notified_effects.try_pop().is_some() {}
    }
    println!(
        "{name}: {count} transactions, {:.1} µs and {} allocations per transaction",
        elapsed.as_secs_f64() * 1e6 / count as f64,
        allocations / count,
    );
}

fn transfers() {
    let mut node = Node::new(TRANSACTIONS);
    let executables = (0..TRANSACTIONS as u64)
        .map(|i| node.executable(node.transfer(node.coin(i)), vec![], vec![]))
        .collect();
    run("transfers", &mut node, executables);
}

fn shared_increments() {
    let mut node = Node::new(TRANSACTIONS + 1);
    let shared = node.new_shared(TRANSACTIONS as u64);
    // One commit round: increment k reads generation k and produces k + 1.
    let executables = (0..TRANSACTIONS as u64)
        .map(|k| {
            let increment = node.increment(node.coin(k), shared);
            node.executable(increment, vec![shared.access(true, 1, k as u32)], vec![])
        })
        .collect();
    run("shared increments", &mut node, executables);
    assert!(
        node.generations
            .get(shared.stream(), 1, TRANSACTIONS as u32)
            .is_some(),
        "every increment produced its generation"
    );
}

fn main() {
    transfers();
    shared_increments();
}
