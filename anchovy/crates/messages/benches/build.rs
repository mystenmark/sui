// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Build, serialize, hash and drop each message a validator produces, with
//! the fast builders against the reference's shape: standard containers on
//! the global heap, the serde mirror types, `bcs::to_bytes`, then a hash
//! over a second serialization as `default_hash` does. Inputs are the
//! parsed views of the mainnet corpus. Run with `cargo bench --bench build`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use messages::Message;
use messages::base::Digest;
use messages::build;
use messages::checkpoint::{CheckpointData, VersionedCheckpointContents};
use messages::effects::VersionedEffects;
use messages::fast::{Bump, ContentsBuilder, EffectsBuilder, EventsBuilder};

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

fn corpus() -> Vec<Message<CheckpointData<'static>>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![manifest.join("tests/data/mainnet-325300367.chk")];
    if let Ok(entries) = std::fs::read_dir(manifest.join("../../corpus/mainnet")) {
        paths = entries.map(|e| e.unwrap().path()).collect();
        paths.retain(|p| p.extension().is_some_and(|e| e == "chk"));
        paths.sort();
    }
    paths
        .iter()
        .map(|p| {
            let mut bytes = std::fs::read(p).unwrap();
            bytes.remove(0);
            Message::<CheckpointData>::parse(bytes).unwrap()
        })
        .collect()
}

/// Per item: the fastest of `rounds`, and the allocations of one round.
struct Measurement {
    time: Duration,
    allocations: f64,
}

fn measure(items: usize, rounds: usize, mut f: impl FnMut()) -> Measurement {
    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..rounds {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    }
    Measurement {
        time: best / items as u32,
        allocations: allocations as f64 / items as f64,
    }
}

fn report(name: &str, fast: &Measurement, reference: &Measurement) {
    println!(
        "{name:<20} fast {:>9.1?} {:>6.2} allocs | reference {:>9.1?} {:>8.2} allocs | {:>6.2}x",
        fast.time,
        fast.allocations,
        reference.time,
        reference.allocations,
        reference.time.as_secs_f64() / fast.time.as_secs_f64()
    );
}

fn effects(checkpoints: &[Message<CheckpointData<'static>>]) {
    let items: usize = checkpoints.iter().map(|c| c.get().transactions.len()).sum();
    let fast = measure(items, 20, || {
        for c in checkpoints {
            for tx in c.get().transactions {
                let VersionedEffects::V2(v2) = &tx.effects.version else {
                    continue;
                };
                let bump = Bump::with_capacity(8192);
                let mut b = EffectsBuilder::new_in(
                    &bump,
                    v2.status,
                    v2.executed_epoch,
                    v2.gas_used,
                    *v2.transaction_digest,
                    v2.lamport_version,
                );
                b.events_digest(v2.events_digest.copied());
                for d in v2.dependencies {
                    b.dependency(*d);
                }
                for (i, ch) in v2.changed_objects.iter().enumerate() {
                    b.change(*ch.id, ch.input_state, ch.output_state, ch.id_operation);
                    if v2.gas_object_index == Some(i as u32) {
                        b.gas_object_is_last();
                    }
                }
                for (id, kind) in v2.unchanged_consensus_objects {
                    b.unchanged(**id, *kind);
                }
                black_box(b.finish());
            }
        }
    });
    let reference = measure(items, 20, || {
        for c in checkpoints {
            for tx in c.get().transactions {
                let VersionedEffects::V2(v2) = &tx.effects.version else {
                    continue;
                };
                // The reference's shape: a sorted set of dependencies, a map
                // of changes by id, a scan for the gas index, then the
                // owned value, its bytes, and a second serialization to hash.
                let dependencies: std::collections::BTreeSet<_> = v2
                    .dependencies
                    .iter()
                    .map(build::base::TransactionDigest::from)
                    .collect();
                let mut changes = BTreeMap::new();
                for ch in v2.changed_objects {
                    changes.insert(
                        build::base::ObjectId::from(ch.id),
                        build::effects::EffectsObjectChange::from(ch),
                    );
                }
                let changed_objects: Vec<_> = changes.into_iter().collect();
                let gas_object_index = v2.gas_object_index.map(|i| {
                    let id = build::base::ObjectId::from(v2.changed_objects[i as usize].id);
                    changed_objects.iter().position(|(k, _)| *k == id).unwrap() as u32
                });
                let mut owned = build::effects::TransactionEffects::from(&tx.effects);
                let build::effects::TransactionEffects::V2(v) = &mut owned else {
                    unreachable!()
                };
                v.dependencies = dependencies.into_iter().collect();
                v.changed_objects = changed_objects;
                v.gas_object_index = gas_object_index;
                let bytes = bcs::to_bytes(&owned).unwrap();
                let digest = Digest::of("TransactionEffects", &bcs::to_bytes(&owned).unwrap());
                black_box((bytes, digest));
            }
        }
    });
    report("TransactionEffects", &fast, &reference);
}

fn events(checkpoints: &[Message<CheckpointData<'static>>]) {
    let items: usize = checkpoints
        .iter()
        .map(|c| {
            c.get()
                .transactions
                .iter()
                .filter(|t| t.events.is_some())
                .count()
        })
        .sum();
    let fast = measure(items, 20, || {
        for c in checkpoints {
            for tx in c.get().transactions {
                let Some(events) = &tx.events else { continue };
                let bump = Bump::with_capacity(4096 + events.bytes.len() * 2);
                let mut b = EventsBuilder::new_in(&bump, events.data.len());
                for e in events.data {
                    b.push(*e);
                }
                black_box(b.finish());
            }
        }
    });
    let reference = measure(items, 20, || {
        for c in checkpoints {
            for tx in c.get().transactions {
                let Some(events) = &tx.events else { continue };
                let owned = build::effects::TransactionEvents {
                    data: events.data.iter().map(Into::into).collect(),
                };
                let bytes = bcs::to_bytes(&owned).unwrap();
                let digest = Digest::of("TransactionEvents", &bcs::to_bytes(&owned).unwrap());
                black_box((bytes, digest));
            }
        }
    });
    report("TransactionEvents", &fast, &reference);
}

fn contents(checkpoints: &[Message<CheckpointData<'static>>]) {
    let fast = measure(checkpoints.len(), 20, || {
        for c in checkpoints {
            let VersionedCheckpointContents::V2(entries) = &c.get().checkpoint_contents.version
            else {
                continue;
            };
            let bump = Bump::with_capacity(c.get().checkpoint_contents.bytes.len() * 3 + 4096);
            let mut signatures: containers::Vec<'_, containers::Vec<'_, (&[u8], Option<u64>)>> =
                containers::Vec::with_capacity_in(entries.len(), &bump);
            for e in *entries {
                let mut sigs = containers::Vec::with_capacity_in(e.user_signatures.len(), &bump);
                sigs.extend(e.user_signatures.iter().map(|(s, v)| (s.0, *v)));
                signatures.push(sigs);
            }
            let mut b = ContentsBuilder::new_in(&bump, entries.len());
            for (i, e) in entries.iter().enumerate() {
                b.push(e.digest.transaction, e.digest.effects, &signatures[i]);
            }
            black_box(b.finish());
        }
    });
    let reference = measure(checkpoints.len(), 20, || {
        for c in checkpoints {
            let owned = build::checkpoint::CheckpointContents::from(&c.get().checkpoint_contents);
            let bytes = bcs::to_bytes(&owned).unwrap();
            let digest = Digest::of("CheckpointContents", &bcs::to_bytes(&owned).unwrap());
            black_box((bytes, digest));
        }
    });
    report("CheckpointContents", &fast, &reference);
}

fn main() {
    let checkpoints = corpus();
    println!(
        "build + serialize + hash + drop, per item, over {} checkpoints",
        checkpoints.len()
    );
    effects(&checkpoints);
    events(&checkpoints);
    contents(&checkpoints);
}
