// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution, transaction by transaction over a store, with anchovy's executor and with sui's:
//! time and allocations per transaction, split into executing and committing.
//!
//! `cargo bench -p execution --bench execute [native|reference] [transfer|create]`, with
//! `TXS` transactions (default 2000). Each workload runs over a fresh store: genesis gives every
//! sender its own gas coin, so the transactions are independent and built up front.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use execution::native::NativeExecution;
use execution::{Execution, Outcome, genesis};
use move_core_types::ident_str;
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::metrics::ExecutionMetrics;
use sui_types::object::Owner;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{CallArg, Transaction, TransactionData};

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

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

// One per run, so the variants' sizes don't matter.
#[allow(clippy::large_enum_variant)]
enum Executor {
    Native(NativeExecution),
    Reference(Execution),
}

impl Executor {
    fn execute(&self, store: &store::Store, transaction: &[u8]) -> Outcome {
        match self {
            Executor::Native(e) => e.execute_bytes(store, transaction).unwrap(),
            Executor::Reference(e) => e.execute(store, transaction).unwrap(),
        }
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    store: store::Store,
    /// Execution does not verify signatures, so one key signs for every sender.
    key: AccountKeyPair,
    accounts: Vec<(SuiAddress, ObjectRef)>,
}

fn setup(reference: &Execution, n: usize) -> Setup {
    let (_, key) = get_key_pair::<AccountKeyPair>();
    let accounts: Vec<SuiAddress> = (0..=n)
        .map(|i| {
            SuiAddress::from(ObjectID::derive_id(
                sui_types::digests::TransactionDigest::new([1; 32]),
                i as u64,
            ))
        })
        .collect();
    let allocations: Vec<(SuiAddress, u64)> = accounts.iter().map(|a| (*a, 10 * SUI)).collect();
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    let objects = genesis::objects(reference, &allocations).unwrap();
    genesis::commit(&store, &objects).unwrap();
    let view = execution::StoreView::new(&store);
    let accounts = accounts
        .into_iter()
        .enumerate()
        .map(|(i, address)| {
            let id = ObjectID::derive_id(
                sui_types::digests::TransactionDigest::genesis_marker(),
                i as u64,
            );
            let gas = view
                .live_object(&id)
                .unwrap()
                .unwrap()
                .compute_object_reference();
            (address, gas)
        })
        .collect();
    Setup {
        _dir: dir,
        store,
        key,
        accounts,
    }
}

fn sign(data: TransactionData, key: &AccountKeyPair) -> Vec<u8> {
    bcs::to_bytes(Transaction::from_data_and_signer(data, vec![key]).data()).unwrap()
}

/// Publishes `object_basics` from the last account, with sui's executor; its package id.
fn publish(setup: &mut Setup, reference: &Execution) -> ObjectID {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../crates/sui-core/src/unit_tests/data/object_basics");
    let package = sui_move_build::BuildConfig::new_for_testing()
        .build(&path)
        .unwrap();
    let (sender, gas) = setup.accounts.pop().unwrap();
    let data = TransactionData::new_module(
        sender,
        gas,
        package.get_package_bytes(false),
        package.get_dependency_storage_package_ids(),
        BUDGET * 10,
        RGP,
    );
    let outcome = reference
        .execute(&setup.store, &sign(data, &setup.key))
        .unwrap();
    let effects: TransactionEffects = bcs::from_bytes(&outcome.effects).unwrap();
    setup.store.commit(outcome.commit).unwrap();
    effects
        .created()
        .into_iter()
        .find(|(_, owner)| *owner == Owner::Immutable)
        .unwrap()
        .0
        .0
}

fn transactions(setup: &mut Setup, reference: &Execution, workload: &str) -> Vec<Vec<u8>> {
    match workload {
        "transfer" => setup
            .accounts
            .iter()
            .map(|(sender, gas)| {
                let data = TransactionData::new_transfer_sui(
                    *sender,
                    *sender,
                    Some(SUI),
                    *gas,
                    BUDGET,
                    RGP,
                );
                sign(data, &setup.key)
            })
            .collect(),
        "create" => {
            let package = publish(setup, reference);
            setup
                .accounts
                .iter()
                .map(|(sender, gas)| {
                    let mut builder = ProgrammableTransactionBuilder::new();
                    builder
                        .move_call(
                            package,
                            ident_str!("object_basics").to_owned(),
                            ident_str!("create").to_owned(),
                            vec![],
                            vec![
                                CallArg::Pure(bcs::to_bytes(&7u64).unwrap()),
                                CallArg::Pure(bcs::to_bytes(sender).unwrap()),
                            ],
                        )
                        .unwrap();
                    let data = TransactionData::new_programmable(
                        *sender,
                        vec![*gas],
                        builder.finish(),
                        BUDGET,
                        RGP,
                    );
                    sign(data, &setup.key)
                })
                .collect()
        }
        other => panic!("unknown workload {other}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let executors: Vec<&str> = match args.first().map(String::as_str) {
        Some(e @ ("native" | "reference")) => vec![e],
        _ => vec!["native", "reference"],
    };
    let workloads: Vec<&str> = match args.get(1).map(String::as_str) {
        Some(w) => vec![w],
        None => vec!["transfer", "create"],
    };
    let n: usize = std::env::var("TXS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);

    let reference =
        Execution::new(ProtocolVersion::MAX.as_u64(), Chain::Unknown, 0, 0, RGP).unwrap();
    println!("executor   workload  txs    execute µs/tx  commit µs/tx  allocs/tx (execute)");
    for workload in &workloads {
        for name in &executors {
            let executor = match *name {
                "native" => Executor::Native(
                    NativeExecution::new(
                        ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown),
                        Arc::new(ExecutionMetrics::new(&prometheus::Registry::new())),
                        0,
                        0,
                        RGP,
                    )
                    .unwrap(),
                ),
                _ => Executor::Reference(
                    Execution::new(ProtocolVersion::MAX.as_u64(), Chain::Unknown, 0, 0, RGP)
                        .unwrap(),
                ),
            };
            let mut setup = setup(&reference, n);
            let txs = transactions(&mut setup, &reference, workload);
            // Warm the VM's package cache, as a running validator's is.
            let warm = txs.len().min(10);
            for tx in &txs[..warm] {
                let outcome = executor.execute(&setup.store, tx);
                setup.store.commit(outcome.commit).unwrap();
            }
            let mut execute = Duration::ZERO;
            let mut commit = Duration::ZERO;
            let mut allocations = 0;
            let mut failed = 0;
            for tx in &txs[warm..] {
                let before = ALLOCATIONS.load(Ordering::Relaxed);
                let start = Instant::now();
                let outcome = executor.execute(&setup.store, tx);
                execute += start.elapsed();
                allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
                let effects: TransactionEffects = bcs::from_bytes(&outcome.effects).unwrap();
                if !effects.status().is_ok() {
                    failed += 1;
                }
                let start = Instant::now();
                setup.store.commit(outcome.commit).unwrap();
                commit += start.elapsed();
            }
            let measured = (txs.len() - warm) as u32;
            println!(
                "{name:<10} {workload:<9} {measured:<6} {:>13.1}  {:>12.1}  {:>9.0}{}",
                (execute / measured).as_secs_f64() * 1e6,
                (commit / measured).as_secs_f64() * 1e6,
                allocations as f64 / f64::from(measured),
                if failed > 0 {
                    format!("  ({failed} failed)")
                } else {
                    String::new()
                },
            );
        }
    }
}
