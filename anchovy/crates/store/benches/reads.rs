// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Reads as the pipeline makes them, one key at a time or in batches: does
//! reading a batch of keys at once (sorted, or spread over threads) beat
//! reading them one by one, and from what batch size? Two sources:
//! - objects, from the object store, as the input waiter reads them: it
//!   holds `OBJECTS` objects, with bytes of real mainnet objects;
//! - transaction digests, looked up in the epoch store, as the commit
//!   handler deduplicates: it holds `OBJECTS / 2` of them, and half the
//!   lookups find nothing, as for a new transaction.
//!
//! Each run opens the store afresh, so the object store's cache starts
//! empty, and reads `READS` distinct random keys, in batches of each size,
//! by each strategy:
//! - one at a time, in arrival order;
//! - sorted by key, then one at a time;
//! - spread over a persistent pool of threads, each reading a share.
//!
//! Warm runs read with the database in the page cache (after a full pass);
//! cold runs drop the page cache first (`sudo -n purge`, macOS), so reads go
//! to the disk; they are skipped if that fails.
//!
//! `cargo bench -p store --bench reads`. The database is kept in
//! `target/bench/reads` (or `$READS_BENCH_DIR`) between runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::checkpoint::CheckpointData;
use store::epoch::{EpochDb, Space};
use store::{Commit, Store, Written};

const OBJECTS: u64 = 2_000_000;
const READS: usize = 200_000;
const BATCHES: [usize; 5] = [1, 16, 64, 256, 1024];
const THREADS: [usize; 2] = [4, 16];

fn id(n: u64) -> ObjectId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&n.to_le_bytes());
    // Spread over the keyspace, as object ids are.
    id[8..16].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
    ObjectId(id)
}

/// Each object `n` at version `n % 7 + 1`.
fn version(n: u64) -> u64 {
    n % 7 + 1
}

/// Bytes of real objects, to cycle through.
fn object_bytes() -> Vec<Vec<u8>> {
    let mut bytes = include_bytes!("../../messages/tests/data/mainnet-325300367.chk").to_vec();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes)
        .map_err(|(e, _)| e)
        .unwrap();
    checkpoint
        .get()
        .transactions
        .iter()
        .flat_map(|tx| tx.output_objects.iter())
        .map(|o| o.bytes.to_vec())
        .collect()
}

fn dir() -> PathBuf {
    std::env::var_os("READS_BENCH_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench/reads"),
        PathBuf::from,
    )
}

/// The object store with `OBJECTS` objects, and the epoch store with the
/// even-numbered ones' ids as processed digests, written once.
fn populate(dir: &Path) {
    let done = dir.join("populated");
    if done.exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(dir);
    let epoch = EpochDb::open(&dir.join("epochs"), 0).unwrap();
    for start in (0..OBJECTS).step_by(10_000) {
        let mut batch = epoch.batch();
        for n in (start..(start + 10_000).min(OBJECTS)).filter(|n| n % 2 == 0) {
            batch.put(Space::Processed, id(n).0.to_vec(), Vec::new());
        }
        batch.commit().unwrap();
    }
    drop(epoch);
    let store = Store::open(&dir.join("db")).unwrap();
    let bytes = object_bytes();
    let started = Instant::now();
    let mut n = 0;
    while n < OBJECTS {
        let written = (n..(n + 10_000).min(OBJECTS))
            .map(|n| Written {
                id: id(n),
                version: version(n),
                digest: Digest::new([n as u8; 32]),
                bytes: bytes[n as usize % bytes.len()].clone(),
            })
            .collect();
        store
            .write_through(Commit {
                written,
                ..Commit::default()
            })
            .unwrap();
        n += 10_000;
    }
    drop(store);
    std::fs::write(done, b"").unwrap();
    println!(
        "wrote {OBJECTS} objects in {:.1}s",
        started.elapsed().as_secs_f64()
    );
}

/// `READS` distinct keys, in random order.
fn keys() -> Vec<(ObjectId, u64)> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::with_capacity(READS);
    while keys.len() < READS {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let n = state % OBJECTS;
        if seen.insert(n) {
            keys.push((id(n), version(n)));
        }
    }
    keys
}

#[derive(Clone, Copy)]
enum Strategy {
    Serial,
    Sorted,
    Threads(usize),
}

impl Strategy {
    fn name(self) -> String {
        match self {
            Strategy::Serial => "one at a time".to_owned(),
            Strategy::Sorted => "sorted".to_owned(),
            Strategy::Threads(t) => format!("{t} threads"),
        }
    }
}

/// Which keys are read.
#[derive(Clone, Copy)]
enum Kind {
    Objects,
    Processed,
}

impl Kind {
    fn open(self, dir: &Path) -> Source {
        match self {
            Kind::Objects => Source::Objects(Store::open(&dir.join("db")).unwrap()),
            Kind::Processed => Source::Processed(EpochDb::open(&dir.join("epochs"), 0).unwrap()),
        }
    }
}

/// Where keys are read.
enum Source {
    Objects(Store),
    Processed(EpochDb),
}

impl Source {
    /// Whether `key` is there.
    fn read(&self, (id, version): &(ObjectId, u64)) -> bool {
        match self {
            Source::Objects(store) => store.object(id, *version).unwrap().is_some(),
            Source::Processed(db) => db.exists(Space::Processed, &id.0).unwrap(),
        }
    }
}

/// Threads that each read the share of a batch sent to them.
struct Pool {
    jobs: Vec<mpsc::Sender<Vec<(ObjectId, u64)>>>,
    done: mpsc::Receiver<usize>,
}

impl Pool {
    fn new(store: &Arc<Source>, threads: usize) -> Pool {
        let (done_tx, done) = mpsc::channel();
        let jobs = (0..threads)
            .map(|_| {
                let (tx, rx) = mpsc::channel::<Vec<(ObjectId, u64)>>();
                let (store, done_tx) = (store.clone(), done_tx.clone());
                std::thread::spawn(move || {
                    for keys in rx {
                        done_tx.send(read_all(&store, &keys)).unwrap();
                    }
                });
                tx
            })
            .collect();
        Pool { jobs, done }
    }

    fn read(&self, batch: &[(ObjectId, u64)]) -> usize {
        let share = batch.len().div_ceil(self.jobs.len());
        let mut sent = 0;
        for (keys, job) in batch.chunks(share).zip(&self.jobs) {
            job.send(keys.to_vec()).unwrap();
            sent += 1;
        }
        (0..sent).map(|_| self.done.recv().unwrap()).sum()
    }
}

fn read_all(store: &Source, keys: &[(ObjectId, u64)]) -> usize {
    keys.iter().filter(|key| store.read(key)).count()
}

/// Reads every key, in batches of `batch`, by `strategy`, from `source`
/// opened afresh, with the page cache dropped first if `cold`; the time it
/// took, and how many were found.
fn run(
    source: Kind,
    dir: &Path,
    keys: &[(ObjectId, u64)],
    batch: usize,
    strategy: Strategy,
    cold: bool,
) -> (Duration, usize) {
    let store = Arc::new(source.open(dir));
    // Opening reads the database's log, which fills the page cache: dropped
    // after it.
    if cold {
        assert!(purge(), "the page cache drops");
    }
    let pool = match strategy {
        Strategy::Threads(t) => Some(Pool::new(&store, t)),
        _ => None,
    };
    let started = Instant::now();
    let mut found = 0;
    for chunk in keys.chunks(batch) {
        found += match strategy {
            Strategy::Serial => read_all(&store, chunk),
            Strategy::Sorted => {
                let mut sorted = chunk.to_vec();
                sorted.sort_unstable_by_key(|(id, version)| (id.0, *version));
                read_all(&store, &sorted)
            }
            Strategy::Threads(_) => pool.as_ref().unwrap().read(chunk),
        };
    }
    (started.elapsed(), found)
}

fn purge() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "purge"])
        .status()
        .is_ok_and(|s| s.success())
}

fn main() {
    let dir = dir();
    populate(&dir);
    let keys = keys();
    let cold = purge();
    if !cold {
        println!("cold runs skipped: `sudo -n purge` failed");
    }
    let mut strategies = vec![Strategy::Serial, Strategy::Sorted];
    strategies.extend(THREADS.map(Strategy::Threads));
    let present = keys.iter().filter(|(id, _)| id.0[0] % 2 == 0).count();
    for (name, kind, expected) in [
        ("objects", Kind::Objects, keys.len()),
        ("processed digests", Kind::Processed, present),
    ] {
        bench(name, kind, &dir, &keys, expected, &strategies, cold);
    }
}

fn bench(
    name: &str,
    source: Kind,
    dir: &Path,
    keys: &[(ObjectId, u64)],
    expected: usize,
    strategies: &[Strategy],
    cold: bool,
) {
    println!("\n{name}: {READS} reads; us per read (reads per second)");
    // Warm the page cache with a full pass.
    run(source, dir, keys, 1024, Strategy::Threads(16), false);
    for (label, purging) in [("warm", false), ("cold", true)] {
        if purging && !cold {
            continue;
        }
        println!("{label}");
        print!("{:>8}", "batch");
        for strategy in strategies {
            print!("{:>22}", strategy.name());
        }
        println!();
        for batch in BATCHES {
            print!("{batch:>8}");
            for &strategy in strategies {
                if batch == 1 && !matches!(strategy, Strategy::Serial) {
                    print!("{:>22}", "-");
                    continue;
                }
                if !purging {
                    run(source, dir, keys, 1024, Strategy::Threads(16), false);
                }
                let (elapsed, found) = run(source, dir, keys, batch, strategy, purging);
                assert_eq!(found, expected, "the keys there are found");
                let per_read = elapsed.as_secs_f64() * 1e6 / READS as f64;
                let rate = READS as f64 / elapsed.as_secs_f64();
                print!("{:>22}", format!("{per_read:.2} ({:.0}k)", rate / 1e3));
            }
            println!();
        }
    }
}
