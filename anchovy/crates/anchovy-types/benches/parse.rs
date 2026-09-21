// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Parse speed and allocation counts over the mainnet corpus
//! (`scripts/fetch-mainnet.sh`). Run with `cargo bench`.
//!
//! The baseline is `bcs::from_bytes` into the owned `build` types, which is
//! how the reference implementation deserializes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anchovy_types::Message;
use anchovy_types::build;
use anchovy_types::checkpoint::CheckpointData;
use anchovy_types::transaction::SenderSignedData;

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

fn push_uleb128(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Re-encodes a parsed transaction. The view keeps the span of the
/// `TransactionData` but not of what surrounds it.
fn encode(tx: &SenderSignedData<'_>) -> Vec<u8> {
    let mut out = vec![1, tx.intent.scope, tx.intent.version, tx.intent.app_id];
    out.extend_from_slice(tx.data.bytes);
    push_uleb128(&mut out, tx.tx_signatures.len());
    for sig in tx.tx_signatures {
        push_uleb128(&mut out, sig.0.len());
        out.extend_from_slice(sig.0);
    }
    out
}

struct Corpus {
    checkpoints: Vec<Vec<u8>>,
    transactions: Vec<Vec<u8>>,
}

fn load() -> Corpus {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![manifest.join("tests/data/mainnet-325300367.chk")];
    if let Ok(entries) = std::fs::read_dir(manifest.join("../../corpus/mainnet")) {
        paths = entries.map(|e| e.unwrap().path()).collect();
        paths.retain(|p| p.extension().is_some_and(|e| e == "chk"));
        paths.sort();
    }
    let mut corpus = Corpus {
        checkpoints: Vec::new(),
        transactions: Vec::new(),
    };
    for path in paths {
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData<'static>>::parse(bytes.clone()).unwrap();
        for tx in checkpoint.get().transactions {
            corpus.transactions.push(encode(&tx.transaction));
        }
        corpus.checkpoints.push(bytes);
    }
    corpus
}

struct Measurement {
    per_item: Duration,
    allocations_per_item: f64,
}

/// Times `f` over every input, the inputs cloned beforehand since parsing
/// consumes its buffer. Reports the fastest of `rounds`.
fn measure<T>(inputs: &[Vec<u8>], rounds: usize, f: impl Fn(Vec<u8>) -> T) -> Measurement {
    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..rounds {
        let owned: Vec<Vec<u8>> = inputs.to_vec();
        let mut outputs = Vec::with_capacity(owned.len());
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for input in owned {
            outputs.push(black_box(f(input)));
        }
        let elapsed = start.elapsed();
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        best = best.min(elapsed);
        drop(outputs);
    }
    Measurement {
        per_item: best / inputs.len() as u32,
        allocations_per_item: allocations as f64 / inputs.len() as f64,
    }
}

fn report(name: &str, inputs: &[Vec<u8>], m: &Measurement) {
    let bytes: usize = inputs.iter().map(Vec::len).sum();
    let mean_len = bytes / inputs.len();
    let mb_per_s = mean_len as f64 / m.per_item.as_secs_f64() / 1e6;
    println!(
        "{name:<44} {:>10.1?}/item {:>8.0} MB/s {:>8.2} allocs/item  ({} items, mean {} bytes)",
        m.per_item,
        mb_per_s,
        m.allocations_per_item,
        inputs.len(),
        mean_len
    );
}

fn main() {
    let corpus = load();

    let m = measure(&corpus.transactions, 30, |b| {
        Message::<SenderSignedData<'static>>::parse(b).unwrap()
    });
    report("SenderSignedData  anchovy", &corpus.transactions, &m);
    let m = measure(&corpus.transactions, 30, |b| {
        Message::<SenderSignedData<'static>>::measure(&b).unwrap()
    });
    report(
        "SenderSignedData  anchovy, measure pass only",
        &corpus.transactions,
        &m,
    );
    let m = measure(&corpus.transactions, 30, |b| {
        bcs::from_bytes::<build::transaction::SenderSignedData>(&b).unwrap()
    });
    report(
        "SenderSignedData  bcs + owned types",
        &corpus.transactions,
        &m,
    );

    let m = measure(&corpus.checkpoints, 10, |b| {
        Message::<CheckpointData<'static>>::parse(b).unwrap()
    });
    report("CheckpointData    anchovy", &corpus.checkpoints, &m);
    let m = measure(&corpus.checkpoints, 10, |b| {
        bcs::from_bytes::<build::checkpoint::CheckpointData>(&b).unwrap()
    });
    report(
        "CheckpointData    bcs + owned types",
        &corpus.checkpoints,
        &m,
    );
}
