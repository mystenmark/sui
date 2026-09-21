// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Parses arbitrary bytes as each root type. The first byte picks the type.
//!
//! Beyond not crashing, a parse may make one allocation, the arena, and the
//! arena may be no more than a fixed multiple of the input.

#![no_main]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use anchovy_types::checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointData, FullCheckpointContents,
};
use anchovy_types::effects::{TransactionEffects, TransactionEvents};
use anchovy_types::object::Object;
use anchovy_types::transaction::{SenderSignedData, TransactionData};
use anchovy_types::{Message, Wire};
use libfuzzer_sys::fuzz_target;

const MAX_ARENA_PER_WIRE_BYTE: usize = 32;
/// Must match `message::MIN_ARENA_GUESS`.
const MIN_ARENA_GUESS: usize = 256;

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: defers to `System` and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn parse<T: Wire>(bytes: &[u8]) {
    let wire = bytes.to_vec();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let result = Message::<T>::parse(wire);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed) - allocated;

    // A guessed arena, and a measured one if the guess fell short.
    assert!(allocations <= 2, "{allocations} allocations");
    assert!(
        allocated <= (bytes.len() + MIN_ARENA_GUESS) * (MAX_ARENA_PER_WIRE_BYTE + 3),
        "{allocated} bytes allocated for {} of input",
        bytes.len()
    );
    if let Ok(m) = result {
        assert!(m.arena_used() <= m.arena_size());
        assert!(m.arena_size() <= allocated);
        assert!(m.arena_used() <= bytes.len() * MAX_ARENA_PER_WIRE_BYTE);
    }
}

fuzz_target!(|input: &[u8]| {
    let Some((&selector, bytes)) = input.split_first() else {
        return;
    };
    match selector % 9 {
        0 => parse::<SenderSignedData<'static>>(bytes),
        1 => parse::<TransactionData<'static>>(bytes),
        2 => parse::<TransactionEffects<'static>>(bytes),
        3 => parse::<TransactionEvents<'static>>(bytes),
        4 => parse::<Object<'static>>(bytes),
        5 => parse::<CheckpointContents<'static>>(bytes),
        6 => parse::<CertifiedCheckpointSummary<'static>>(bytes),
        7 => parse::<FullCheckpointContents<'static>>(bytes),
        _ => parse::<CheckpointData<'static>>(bytes),
    }
});
