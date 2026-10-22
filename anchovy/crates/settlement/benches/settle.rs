// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Settling chunks of 100 and 1000 transactions, each moving SUI between
//! two of as many accounts as there are transactions. Time and allocations
//! per chunk transaction, for two spans:
//! - `build`: the settlement transactions from parsed effects (merging the
//!   accumulator events, building and serializing the transactions,
//!   hashing their digests);
//! - `process`: `SettlementScheduler` from `ChunkExecuted` to the settled
//!   chunk, store reads of every effects included; the store writes that
//!   stand in for execution happen outside the measurement.
//!
//! Run with `cargo bench -p settlement --bench settle`. On an Apple
//! M-series laptop, with the protocol's 100 updates per settlement
//! transaction:
//!
//! ```text
//! span     chunk    ns/tx  allocs/tx
//! build    100        753      19.61
//! process  100       2406      36.62
//! build    1000       764      19.60
//! process  1000      2193      35.86
//! ```
//!
//! Building costs about ten allocations per settled account (its owned
//! type tag, three pure inputs, the Move call); the rest of `process` is
//! the store reads and parses of each transaction's effects and copying
//! them into the chunk table.

#[path = "../tests/support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::effects::{TransactionEffects, VersionedEffects};
use messages::transaction::SharedObjectMutability;
use protocol_config::{Chain, ProtocolVersion};
use settlement::SettlementScheduler;
use settlement::settle::{self, SettlementBuilder};
use validator::consensus::sequence::{SharedAccess, SharedOrdinal};
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    ChunkSettlement, EffectsWaitItem, Executable, ExecutableTransaction, SettlementInput,
};
use validator::pipeline::keys::AccumulatorVersion;
use validator::pipeline::tables::ChunkTable;
use waiter::Command;
use workqueue::{Inbox, Processor};

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

const ISV: u64 = 1234;

/// Times `f`, returning its result, the time and the allocations.
fn measure<R>(f: impl FnOnce() -> R) -> (R, Duration, usize) {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let start = Instant::now();
    let r = f();
    let took = start.elapsed();
    (r, took, ALLOCATIONS.load(Ordering::Relaxed) - before)
}

fn report(span: &str, size: usize, rounds: usize, elapsed: Duration, allocations: usize) {
    let per = (rounds * size) as f64;
    println!(
        "{span:<7}  {size:<5}  {:>7.0}  {:>9.2}",
        elapsed.as_nanos() as f64 / per,
        allocations as f64 / per,
    );
}

fn parse(bytes: &[Vec<u8>]) -> Vec<Message<TransactionEffects<'static>>> {
    bytes
        .iter()
        .map(|b| Message::parse(b.clone()).map_err(|(e, _)| e).unwrap())
        .collect()
}

fn bench_build(size: usize, effects: &[Vec<u8>], max_updates: Option<u32>) {
    let parsed = parse(effects);
    let rounds = 50;
    let (mut elapsed, mut allocations) = (Duration::ZERO, 0);
    for round in 0..=rounds {
        let bump = containers::Bump::with_capacity(64 * 1024);
        let (built, took, allocated) = measure(|| {
            let mut builder = SettlementBuilder::new(1, 0, &bump);
            for (i, e) in parsed.iter().enumerate() {
                builder.add(i as u64, e.get());
            }
            builder
                .build(max_updates, 7, ISV, 1)
                .into_iter()
                .map(settle::system_transaction)
                .collect::<Vec<_>>()
        });
        assert!(!built.is_empty());
        if round > 0 {
            elapsed += took;
            allocations += allocated;
        }
    }
    report("build", size, rounds, elapsed, allocations);
}

/// The items queued in `inbox` as waits, ignoring resolutions.
fn drain<K, T, V>(inbox: &Inbox<Command<K, T, V>>) -> Vec<T> {
    let mut items = Vec::new();
    while let Some(command) = inbox.try_pop() {
        if let Command::Wait(batch) = command {
            items.extend(batch.into_parts().into_iter().map(|(item, _)| item));
        }
    }
    items
}

fn commit(store: &store::Store, effects: &[u8]) {
    let view = Message::<TransactionEffects<'static>>::parse(effects.to_vec())
        .map_err(|(e, _)| e)
        .unwrap();
    let VersionedEffects::V2(v2) = &view.get().version else {
        unreachable!()
    };
    store
        .write_through(store::Commit {
            executed: Some(store::Executed {
                digest: *v2.transaction_digest,
                transaction: Vec::new(),
                effects_digest: (*view.digest()).into(),
                effects: effects.to_vec(),
                events: None,
            }),
            ..store::Commit::default()
        })
        .unwrap();
}

fn digest_of(executable: &Executable) -> [u8; 32] {
    match &executable.entry.transaction {
        ExecutableTransaction::System(system) => system.digest,
        ExecutableTransaction::User(_) => unreachable!(),
    }
}

fn bench_process(size: usize, effects: &[Vec<u8>]) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    store
        .write_through(store::Commit {
            written: vec![store::Written {
                id: ObjectId(support::ROOT),
                version: ISV,
                digest: Digest::new([1; 32]),
                bytes: support::root_object(ISV),
            }],
            ..store::Commit::default()
        })
        .unwrap();
    for e in effects {
        commit(&store, e);
    }
    let digests: Vec<[u8; 32]> = parse(effects)
        .iter()
        .map(|m| match &m.get().version {
            VersionedEffects::V2(v2) => v2.transaction_digest.bytes,
            VersionedEffects::V1(_) => unreachable!(),
        })
        .collect();
    let epoch = Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        7,
        Digest::ZERO,
        1000,
        4,
    ));
    let (effects_q, effects_in) = workqueue::queue(64);
    let (inputs_q, inputs_in) = workqueue::queue(64);
    let (checkpoints_q, checkpoints_in) = workqueue::queue(64);
    let (funds_q, funds_in) = workqueue::queue(64);
    let mut scheduler = SettlementScheduler::new(
        epoch.clone(),
        store.clone(),
        Arc::new(ChunkTable::new()),
        effects_q,
        inputs_q,
        checkpoints_q,
        funds_q,
    );
    let rounds = 20;
    let (mut elapsed, mut allocations) = (Duration::ZERO, 0);
    for round in 0..=rounds {
        let height = round as u64 + 1;
        let chunk = ChunkSettlement {
            version: AccumulatorVersion(height),
            root_version: height,
            last_of_commit: true,
            round: height as u32,
            height,
            checkpoint_seq: height,
            digests: digests.clone(),
            root: SharedAccess {
                id: ObjectId(support::ROOT),
                initial_shared_version: ISV,
                mutability: SharedObjectMutability::Mutable,
                ordinal: SharedOrdinal {
                    round: height as u32,
                    writes_before: 0,
                },
                base: None,
            },
        };
        scheduler.process(
            SettlementInput::Chunk(chunk.clone()),
            &mut workqueue::Bump::default(),
        );
        drain(&effects_in);

        let ((), mut took, mut allocated) = measure(|| {
            _ = scheduler.process(
                SettlementInput::ChunkExecuted(chunk),
                &mut workqueue::Bump::default(),
            );
        });
        let settlements: Vec<[u8; 32]> = drain(&inputs_in).iter().map(digest_of).collect();
        drain(&effects_in);
        for d in &settlements {
            commit(&store, &support::effects(*d, 2 * height, &[], vec![]));
        }
        let ((), t, a) = measure(|| {
            _ = scheduler.process(
                SettlementInput::SettlementExecuted {
                    height,
                    digests: settlements,
                },
                &mut workqueue::Bump::default(),
            );
        });
        took += t;
        allocated += a;
        let barrier = digest_of(&drain(&inputs_in)[0]);
        drain(&effects_in);
        commit(
            &store,
            &support::effects(
                barrier,
                2 * height + 1,
                &[],
                vec![support::root_mutated(ISV, 2 * height)],
            ),
        );
        let ((), t, a) = measure(|| {
            _ = scheduler.process(
                SettlementInput::SettlementExecuted {
                    height,
                    digests: vec![barrier],
                },
                &mut workqueue::Bump::default(),
            );
        });
        took += t;
        allocated += a;
        assert!(matches!(
            drain::<_, EffectsWaitItem, ()>(&effects_in)[..],
            []
        ));
        while checkpoints_in.try_pop().is_some() {}
        while funds_in.try_pop().is_some() {}
        if round > 0 {
            elapsed += took;
            allocations += allocated;
        }
    }
    report("process", size, rounds, elapsed, allocations);
}

fn main() {
    let max_updates =
        protocol_config::ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown)
            .max_updates_per_settlement_txn;
    println!("span     chunk    ns/tx  allocs/tx");
    for size in [100, 1000] {
        let effects = support::transfers(size as u64, size as u64);
        bench_build(size, &effects, max_updates);
        bench_process(size, &effects);
    }
}
