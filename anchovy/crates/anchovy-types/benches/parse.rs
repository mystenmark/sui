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
use anchovy_types::checkpoint::{CheckpointContents, CheckpointData, CheckpointSummary};
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
    /// Full checkpoint downloads: summary, contents, and every transaction
    /// with its effects, events and objects.
    checkpoint_data: Vec<Vec<u8>>,
    /// The checkpoints themselves: a header, and a list of digests.
    summaries: Vec<Vec<u8>>,
    contents: Vec<Vec<u8>>,
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
        checkpoint_data: Vec::new(),
        summaries: Vec::new(),
        contents: Vec::new(),
        transactions: Vec::new(),
    };
    for path in paths {
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData<'static>>::parse(bytes.clone()).unwrap();
        let view = checkpoint.get();
        corpus
            .summaries
            .push(view.checkpoint_summary.data.bytes.to_vec());
        corpus
            .contents
            .push(view.checkpoint_contents.bytes.to_vec());
        for tx in view.transactions {
            corpus.transactions.push(encode(&tx.transaction));
        }
        corpus.checkpoint_data.push(bytes);
    }
    corpus
}

/// Per item.
struct Measurement {
    parse: Duration,
    /// Dropping the parsed value, its input buffer included.
    drop: Duration,
    allocations: f64,
}

/// Times `f` over every input, the inputs cloned beforehand since parsing
/// consumes its buffer, and then times dropping everything `f` returned.
/// Reports the fastest of `rounds` for each.
fn measure<T>(inputs: &[Vec<u8>], rounds: usize, f: impl Fn(Vec<u8>) -> T) -> Measurement {
    let mut best_parse = Duration::MAX;
    let mut best_drop = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..rounds {
        let owned: Vec<Vec<u8>> = inputs.to_vec();
        let mut outputs = Vec::with_capacity(owned.len());
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for input in owned {
            outputs.push(black_box(f(input)));
        }
        best_parse = best_parse.min(start.elapsed());
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;

        let start = Instant::now();
        drop(outputs);
        best_drop = best_drop.min(start.elapsed());
    }
    Measurement {
        parse: best_parse / inputs.len() as u32,
        drop: best_drop / inputs.len() as u32,
        allocations: allocations as f64 / inputs.len() as f64,
    }
}

fn report(name: &str, inputs: &[Vec<u8>], m: &Measurement) {
    let bytes: usize = inputs.iter().map(Vec::len).sum();
    let mean_len = bytes / inputs.len();
    let total = m.parse + m.drop;
    let mb_per_s = mean_len as f64 / total.as_secs_f64() / 1e6;
    println!(
        "{name:<44} parse {:>9.1?}  drop {:>9.1?}  total {:>9.1?} {:>7.0} MB/s {:>8.2} allocs  ({} items, mean {} bytes)",
        m.parse,
        m.drop,
        total,
        mb_per_s,
        m.allocations,
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
        Message::<SenderSignedData<'static>>::parse_exact(b).unwrap()
    });
    report(
        "SenderSignedData  anchovy, exact two-pass",
        &corpus.transactions,
        &m,
    );
    // The baseline keeps its input buffer too, so that both drops free it.
    // It does not hash: the reference computes digests separately, on
    // demand, so anchovy's parse is doing more than the baseline's.
    let m = measure(&corpus.transactions, 30, |b| {
        (
            bcs::from_bytes::<build::transaction::SenderSignedData>(&b).unwrap(),
            b,
        )
    });
    report(
        "SenderSignedData  bcs + owned types",
        &corpus.transactions,
        &m,
    );

    let m = measure(&corpus.summaries, 30, |b| {
        Message::<CheckpointSummary<'static>>::parse(b).unwrap()
    });
    report("CheckpointSummary anchovy", &corpus.summaries, &m);
    let m = measure(&corpus.summaries, 30, |b| {
        (
            bcs::from_bytes::<build::checkpoint::CheckpointSummary>(&b).unwrap(),
            b,
        )
    });
    report("CheckpointSummary bcs + owned types", &corpus.summaries, &m);

    let m = measure(&corpus.contents, 30, |b| {
        Message::<CheckpointContents<'static>>::parse(b).unwrap()
    });
    report("CheckpointContents anchovy", &corpus.contents, &m);
    let m = measure(&corpus.contents, 30, |b| {
        (
            bcs::from_bytes::<build::checkpoint::CheckpointContents>(&b).unwrap(),
            b,
        )
    });
    report("CheckpointContents bcs + owned types", &corpus.contents, &m);

    let m = measure(&corpus.checkpoint_data, 10, |b| {
        Message::<CheckpointData<'static>>::parse(b).unwrap()
    });
    report("CheckpointData    anchovy", &corpus.checkpoint_data, &m);
    let m = measure(&corpus.checkpoint_data, 10, |b| {
        (
            bcs::from_bytes::<build::checkpoint::CheckpointData>(&b).unwrap(),
            b,
        )
    });
    report(
        "CheckpointData    bcs + owned types",
        &corpus.checkpoint_data,
        &m,
    );
}
