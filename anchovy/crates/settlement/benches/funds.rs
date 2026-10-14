// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The funds ledger under load: commits of 1000 withdrawing transactions
//! against 10 hot accounts or 100,000 cold ones, each reserving against the
//! commit's version while the previous commit is still unsettled. Each
//! transaction withdraws half its reservation, which is credited back, and
//! deposits into another account; then the previous version settles.
//! Balances are `rich` (every reservation fits the lower bound) or `tight`
//! (most wait for settlement). Time and allocations per reservation, every
//! event included, and the share fulfilled before its version settled.
//!
//! Run with `cargo bench -p settlement --bench funds`. On an Apple M-series
//! laptop:
//!
//! ```text
//! accounts  balances   ns/res  allocs/res  early
//! 10        rich           39        0.02   100%
//! 10        tight          21        0.07     1%
//! 100000    rich          275        5.86   100%
//! 100000    tight         270        5.85    99%
//! ```
//!
//! With cold accounts nearly every reservation creates its account's state
//! (a map entry, a version level, a queue) and settlement drops it again:
//! that is the allocations, and most of the time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use messages::base::ObjectId;
use settlement::funds::Ledger;
use validator::pipeline::items::ReservationStatus;

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

const COMMIT: u64 = 1000;
const AMOUNT: u64 = 100;

/// A small deterministic generator, so runs compare.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn account(n: u64) -> ObjectId {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    ObjectId(bytes)
}

fn bench(accounts: u64, balance: u128, label: &str) {
    let commits = 50;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut ledger = Ledger::new();
    let mut out = Vec::with_capacity(4 * COMMIT as usize);
    let mut base = |_: &ObjectId| balance;
    ledger.settle(0, &[], &mut base, &mut out);
    let (mut elapsed, mut allocations, mut early, mut reserved) = (Duration::ZERO, 0, 0, 0);
    let mut ticket = 0;
    // Each reservation's outcome, by ticket.
    let mut outcomes: Vec<Option<ReservationStatus>> = Vec::new();
    // Each commit's withdrawals and deposits, to credit and settle later.
    let mut previous: Vec<(ObjectId, ObjectId)> = Vec::new();
    for version in 1..=commits {
        let current: Vec<(ObjectId, ObjectId)> = (0..COMMIT)
            .map(|_| (account(rng.below(accounts)), account(rng.below(accounts))))
            .collect();
        outcomes.resize(outcomes.len() + COMMIT as usize, None);
        let first = ticket;
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for &(from, _) in &current {
            ledger.reserve(ticket, from, AMOUNT, version, &mut base, &mut out);
            ticket += 1;
        }
        // The previous commit executes: its fulfilled reservations withdraw
        // half, it deposits, and its version settles.
        let previous_first = first.saturating_sub(COMMIT);
        for (i, &(from, to)) in previous.iter().enumerate() {
            if outcomes[(previous_first + i as u64) as usize] == Some(ReservationStatus::Fulfilled)
            {
                ledger.credit(from, AMOUNT / 2, version - 1, &mut out);
                ledger.deposit(to, AMOUNT / 2, version, &mut base, &mut out);
            }
            // Outcomes resolved so far, before the settlement.
            for (t, status) in out.drain(..) {
                early += usize::from(status == ReservationStatus::Fulfilled);
                outcomes[t as usize] = Some(status);
            }
        }
        for (t, status) in out.drain(..) {
            early += usize::from(status == ReservationStatus::Fulfilled);
            outcomes[t as usize] = Some(status);
        }
        ledger.settle(version, &[], &mut base, &mut out);
        for (t, status) in out.drain(..) {
            outcomes[t as usize] = Some(status);
        }
        elapsed += start.elapsed();
        allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
        reserved += COMMIT as usize;
        previous = current;
    }
    let per = reserved as f64;
    println!(
        "{accounts:<8}  {label:<8}  {:>7.0}  {:>10.2}  {:>4.0}%",
        elapsed.as_nanos() as f64 / per,
        allocations as f64 / per,
        100.0 * early as f64 / per,
    );
}

fn main() {
    println!("accounts  balances   ns/res  allocs/res  early");
    for accounts in [10, 100_000] {
        bench(accounts, u128::from(AMOUNT) * 1_000_000, "rich");
        bench(accounts, u128::from(AMOUNT), "tight");
    }
}
