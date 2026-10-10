// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Items waiting on one to four keys each, the size of an object version
//! (an id and a version), keys shared between items; then every key
//! notified, in shuffled batches. Time per item waited on and per key
//! notified, and allocations, over rounds after a first that sizes the
//! tables. Then a matrix of fan-in (keys per item) by fan-out (items per
//! key) at the same number of item-key edges. Run with
//! `cargo bench -p waiter`; `ITEMS` sets the mixed run's item count, `EDGES`
//! the matrix's edges, `BATCH` the batch size, and `VISIBLE` the percentage
//! of keys the mixed run's check finds available already.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use waiter::{WaitBatch, Waiter};

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

/// An object id and version.
type Key = ([u8; 32], u64);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn key(i: usize, round: u64) -> Key {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&(i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
    id[8..16].copy_from_slice(&(i as u64).to_le_bytes());
    (id, round)
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A shuffle, in place.
fn shuffle<T>(items: &mut [T], rng: &mut Rng) {
    for i in (1..items.len()).rev() {
        items.swap(i, rng.below(i + 1));
    }
}

/// Times `item_keys` (each item's keys, by index below `key_count`) waited
/// on, then every key notified in shuffled batches, over three rounds; the
/// last round's numbers, once the tables are sized.
struct Measured {
    wait_per_item: f64,
    wait_per_edge: f64,
    notify_per_key: f64,
    notify_per_edge: f64,
    allocs_per_item: f64,
}

fn measure(
    item_keys: &[Vec<usize>],
    key_count: usize,
    visible: &[bool],
    batch_size: usize,
    rng: &mut Rng,
) -> Measured {
    let items = item_keys.len();
    let edges: usize = item_keys.iter().map(Vec::len).sum();
    let mut order: Vec<usize> = (0..key_count).collect();
    shuffle(&mut order, rng);
    // The check finds a key visible by its index, which the id holds.
    let mut waiter: Waiter<Key, u32, _> = Waiter::new(|key: &Key| {
        let i = u64::from_le_bytes(key.0[8..16].try_into().unwrap()) as usize;
        visible[i]
    });
    let mut batch = WaitBatch::with_capacity(batch_size, batch_size * 4);
    let mut notified = Vec::with_capacity(batch_size);
    let mut ready = Vec::with_capacity(items);
    let mut measured = None;
    for round in 0..3u64 {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let mut waiting = Duration::ZERO;
        for (chunk_start, chunk) in item_keys.chunks(batch_size).enumerate() {
            for (j, keys) in chunk.iter().enumerate() {
                batch.push(
                    (chunk_start * batch_size + j) as u32,
                    keys.iter().map(|k| key(*k, round)),
                );
            }
            let t = Instant::now();
            waiter.wait_for(&mut batch);
            waiting += t.elapsed();
        }
        let mut notifying = Duration::ZERO;
        for chunk in order.chunks(batch_size) {
            notified.clear();
            notified.extend(chunk.iter().map(|k| key(*k, round)));
            let t = Instant::now();
            waiter.notify(&notified);
            notifying += t.elapsed();
        }
        waiter.get_ready(&mut ready);
        assert_eq!(ready.len(), items, "every item ready");
        assert_eq!(waiter.waiting(), 0);
        ready.clear();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        measured = Some(Measured {
            wait_per_item: waiting.as_nanos() as f64 / items as f64,
            wait_per_edge: waiting.as_nanos() as f64 / edges as f64,
            notify_per_key: notifying.as_nanos() as f64 / key_count as f64,
            notify_per_edge: notifying.as_nanos() as f64 / edges as f64,
            allocs_per_item: allocations as f64 / items as f64,
        });
    }
    measured.expect("three rounds")
}

fn main() {
    let items = env("ITEMS", 1_000_000);
    let batch_size = env("BATCH", 1_000);
    let visible_percent = env("VISIBLE", 0);
    let edges = env("EDGES", 2_000_000);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);

    // Mixed: one to four keys per item, about two items per key.
    let key_count = items;
    let item_keys: Vec<Vec<usize>> = (0..items)
        .map(|_| (0..=rng.below(4)).map(|_| rng.below(key_count)).collect())
        .collect();
    let visible: Vec<bool> = (0..key_count)
        .map(|_| rng.below(100) < visible_percent)
        .collect();
    let m = measure(&item_keys, key_count, &visible, batch_size, &mut rng);
    println!(
        "mixed: {items} items, 1-4 keys each, about 2 items per key, {visible_percent}% visible"
    );
    println!(
        "  wait {:.1} ns/item, notify {:.1} ns/key, {:.3} allocs/item\n",
        m.wait_per_item, m.notify_per_key, m.allocs_per_item
    );

    // Fan-in (keys per item) by fan-out (items per key), the same number of
    // item-key edges each: every key used by exactly `fan_out` items, dealt
    // out at random, nothing visible.
    println!("{edges} edges, nothing visible");
    println!(
        "fan-in  fan-out  items     keys      wait ns/item  wait ns/edge  notify ns/key  notify ns/edge  allocs/item"
    );
    for fan_in in [1, 8, 64, 512] {
        for fan_out in [1, 8, 64, 512] {
            let key_count = edges / fan_out;
            let mut slots: Vec<usize> = (0..key_count)
                .flat_map(|k| std::iter::repeat_n(k, fan_out))
                .collect();
            shuffle(&mut slots, &mut rng);
            let item_keys: Vec<Vec<usize>> = slots.chunks(fan_in).map(<[usize]>::to_vec).collect();
            let visible = vec![false; key_count];
            let m = measure(&item_keys, key_count, &visible, batch_size, &mut rng);
            println!(
                "{fan_in:<6}  {fan_out:<7}  {:<8}  {key_count:<8}  {:>12.1}  {:>12.1}  {:>13.1}  {:>14.1}  {:>11.3}",
                item_keys.len(),
                m.wait_per_item,
                m.wait_per_edge,
                m.notify_per_key,
                m.notify_per_edge,
                m.allocs_per_item,
            );
        }
    }
}
