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
        let checkpoint = Message::<CheckpointData>::parse(bytes.clone()).unwrap();
        let view = checkpoint.get();
        corpus
            .summaries
            .push(view.checkpoint_summary.data.bytes.to_vec());
        corpus
            .contents
            .push(view.checkpoint_contents.bytes.to_vec());
        for tx in view.transactions {
            corpus.transactions.push(tx.transaction.bytes.to_vec());
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
/// Reports the round with the fastest parse-and-drop.
fn measure<T>(inputs: &[Vec<u8>], rounds: usize, f: impl Fn(Vec<u8>) -> T) -> Measurement {
    let mut best = (Duration::MAX, Duration::ZERO);
    let mut allocations = 0;
    for _ in 0..rounds {
        let owned: Vec<Vec<u8>> = inputs.to_vec();
        let mut outputs = Vec::with_capacity(owned.len());
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for input in owned {
            outputs.push(black_box(f(input)));
        }
        let parse = start.elapsed();
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;

        let start = Instant::now();
        drop(outputs);
        let drop = start.elapsed();
        if parse + drop < best.0 + best.1 {
            best = (parse, drop);
        }
    }
    Measurement {
        parse: best.0 / inputs.len() as u32,
        drop: best.1 / inputs.len() as u32,
        allocations: allocations as f64 / inputs.len() as f64,
    }
}

fn header() {
    println!(
        "{:<28} {:>6} {:>6} | {:<32} | {:<32} | {:>8}",
        "", "items", "bytes", "anchovy", "bcs + owned types", "speed-up"
    );
    let columns = format!(
        "{:>8} {:>8} {:>8} {:>6}",
        "parse", "drop", "total", "allocs"
    );
    println!(
        "{:<28} {:>6} {:>6} | {columns} | {columns} | {:>8}",
        "", "", "mean", "total"
    );
}

fn cell(m: &Measurement) -> String {
    format!(
        "{:>8.1?} {:>8.1?} {:>8.1?} {:>6.1}",
        m.parse,
        m.drop,
        m.parse + m.drop,
        m.allocations
    )
}

/// One row: anchovy against the baseline on the same inputs. The last
/// column is the baseline's parse-and-drop time over anchovy's.
fn compare<A, B>(
    name: &str,
    inputs: &[Vec<u8>],
    rounds: usize,
    anchovy: impl Fn(Vec<u8>) -> A,
    baseline: impl Fn(Vec<u8>) -> B,
) {
    let a = measure(inputs, rounds, anchovy);
    let b = measure(inputs, rounds, baseline);
    let speed_up = (b.parse + b.drop).as_secs_f64() / (a.parse + a.drop).as_secs_f64();
    let mean_len = inputs.iter().map(Vec::len).sum::<usize>() / inputs.len();
    println!(
        "{name:<28} {:>6} {:>6} | {} | {} | {speed_up:>7.2}x",
        inputs.len(),
        mean_len,
        cell(&a),
        cell(&b)
    );
}

/// The baseline keeps its input buffer, so that both drops free it. It
/// does not hash: the reference computes digests on demand, not while
/// deserializing, so anchovy's parse does more than the baseline's.
fn baseline<T: serde::de::DeserializeOwned>(b: Vec<u8>) -> (T, Vec<u8>) {
    (bcs::from_bytes::<T>(&b).unwrap(), b)
}

fn main() {
    let corpus = load();
    header();
    compare(
        "SenderSignedData",
        &corpus.transactions,
        30,
        |b| Message::<SenderSignedData>::parse(b).unwrap(),
        baseline::<build::transaction::SenderSignedData>,
    );
    compare(
        "SenderSignedData, exact",
        &corpus.transactions,
        30,
        |b| Message::<SenderSignedData>::parse_exact(b).unwrap(),
        baseline::<build::transaction::SenderSignedData>,
    );
    compare(
        "CheckpointSummary",
        &corpus.summaries,
        30,
        |b| Message::<CheckpointSummary>::parse(b).unwrap(),
        baseline::<build::checkpoint::CheckpointSummary>,
    );
    compare(
        "CheckpointContents",
        &corpus.contents,
        30,
        |b| Message::<CheckpointContents>::parse(b).unwrap(),
        baseline::<build::checkpoint::CheckpointContents>,
    );
    compare(
        "CheckpointData",
        &corpus.checkpoint_data,
        10,
        |b| Message::<CheckpointData>::parse(b).unwrap(),
        baseline::<build::checkpoint::CheckpointData>,
    );
}
