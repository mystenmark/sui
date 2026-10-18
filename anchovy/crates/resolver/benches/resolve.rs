// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Resolving chunks of 100 and 1000 transactions, each with an owned input,
//! a gas coin, a call into one package, and zero to three shared inputs
//! drawn from a few hot objects or from many, written or read at random;
//! one chunk per round, so generation 0 often resolves to an earlier
//! round's generation. Then the input waiter's availability check on
//! batches of keys: owned versions half present, packages, generations and
//! live streams. Time and allocations per transaction or key. Run with
//! `cargo bench -p resolver --bench resolve`.
//!
//! On an M-series Mac:
//!
//! ```text
//! chunk  objects  ns/tx  allocs/tx
//! 100    10        81.1        0.1
//! 100    100000   102.8        0.8
//! 1000   10        67.4        0.0
//! 1000   100000   106.3        0.8
//! batch  ns/key  allocs/key
//! 100     160.0         2.9
//! 1000    148.9         2.8
//! ```
//!
//! An allocation per transaction is its `resolved_base`, when it has one:
//! with many objects, most of a transaction's streams were last seen in an
//! earlier round, read live or written. No transaction withdraws funds, so
//! none computes withdrawal amounts.
//!
//! The check's allocations are the store's: each `ObjectVersion` reads and
//! parses the object, each `Package` reads its live entry.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::transaction::SharedObjectMutability;
use protocol_config::{Chain, ProtocolVersion};
use resolver::{InputAvailability, VersionResolver};
use sui_types::Identifier;
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::object::{Object, Owner};
use sui_types::transaction::{
    CallArg, Command as SuiCommand, ObjectArg, ProgrammableTransaction, Transaction,
    TransactionData, TransactionKind,
};
use validator::checks;
use validator::consensus::sequence::{SharedAccess, SharedOrdinal};
use validator::epoch::EpochParams;
use validator::pipeline::items::{Chunk, ExecutableTransaction, ScheduledEntry};
use validator::pipeline::keys::{AccumulatorVersion, InputKey, StreamId};
use validator::pipeline::tables::{Generation, GenerationTable};
use waiter::{Availability, Command};
use workqueue::Processor;

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

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// An empty generation table, over an epoch store in a directory kept alive
/// with it.
fn generation_table() -> (Arc<GenerationTable>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (store, _, _) = validator::epoch_store::EpochStore::open(dir.path(), 0);
    (Arc::new(GenerationTable::new(store)), dir)
}

fn object_id(n: u64) -> ObjectId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&n.to_le_bytes());
    ObjectId(id)
}

/// A transaction as `Transaction` BCS, with its shared inputs: (object,
/// written).
type Planned = (Vec<u8>, Vec<(u64, bool)>);

/// A chunk's transactions.
fn transactions(count: usize, objects: u64, rng: &mut Rng) -> Vec<Planned> {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let package = ObjectID::new([0x50; 32]);
    (0..count)
        .map(|_| {
            let mut shared: Vec<(u64, bool)> = Vec::new();
            for _ in 0..rng.below(4) {
                let object = rng.below(objects);
                if shared.iter().all(|(o, _)| *o != object) {
                    shared.push((object, rng.below(2) == 0));
                }
            }
            let pt = ProgrammableTransaction {
                inputs: vec![CallArg::Object(ObjectArg::ImmOrOwnedObject(
                    random_object_ref(),
                ))],
                commands: vec![SuiCommand::move_call(
                    package,
                    Identifier::new("m").unwrap(),
                    Identifier::new("f").unwrap(),
                    vec![],
                    vec![],
                )],
            };
            let data = TransactionData::new(
                TransactionKind::ProgrammableTransaction(pt),
                sender,
                random_object_ref(),
                1_000_000,
                1000,
            );
            let bytes =
                bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap();
            (bytes, shared)
        })
        .collect()
}

/// The chunk of `round`, with ordinals counting exclusive writes.
fn chunk(round: u32, transactions: &[Planned]) -> Chunk {
    let mut writes: HashMap<u64, u32> = HashMap::new();
    let entries = transactions
        .iter()
        .map(|(bytes, shared)| {
            let shared = shared
                .iter()
                .map(|&(object, written)| {
                    let count = writes.entry(object).or_default();
                    let writes_before = *count;
                    if written {
                        *count += 1;
                    }
                    SharedAccess {
                        id: object_id(object),
                        initial_shared_version: 1,
                        mutability: if written {
                            SharedObjectMutability::Mutable
                        } else {
                            SharedObjectMutability::Immutable
                        },
                        ordinal: SharedOrdinal {
                            round,
                            writes_before,
                        },
                        base: None,
                    }
                })
                .collect();
            let transaction = checks::sequenced_by_consensus(
                Message::parse(bytes.clone()).map_err(|(e, _)| e).unwrap(),
            );
            ScheduledEntry {
                transaction: ExecutableTransaction::User(transaction),
                shared,
                origin: None,
            }
        })
        .collect();
    Chunk {
        version: AccumulatorVersion(u64::from(round)),
        root_version: 1,
        round,
        height: u64::from(round),
        checkpoint_seq: u64::from(round),
        entries,
    }
}

fn resolve(epoch: &Arc<EpochParams>, rng: &mut Rng) {
    println!("chunk  objects  ns/tx  allocs/tx");
    for chunk_size in [100, 1000] {
        for objects in [10, 100_000] {
            let transactions = transactions(chunk_size, objects, rng);
            let (inputs, inbox) = workqueue::queue(4);
            let (withdrawals, _) = workqueue::queue(4);
            let mut resolver = VersionResolver::new(epoch.clone(), inputs, withdrawals);
            let mut elapsed = Duration::ZERO;
            let mut allocations = 0;
            let rounds = 200;
            for round in 0..=rounds {
                let chunk = chunk(round, &transactions);
                let before = ALLOCATIONS.load(Ordering::Relaxed);
                let start = Instant::now();
                resolver.process(chunk);
                let took = start.elapsed();
                let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
                let Some(Command::Wait(batch)) = inbox.try_pop() else {
                    panic!("a wait");
                };
                assert_eq!(batch.len(), chunk_size);
                drop(batch);
                // The first round sizes the scratch and the stream map.
                if round > 0 {
                    elapsed += took;
                    allocations += allocated;
                }
            }
            let per = f64::from(rounds) * chunk_size as f64;
            println!(
                "{chunk_size:<5}  {objects:<7}  {:>5.1}  {:>9.1}",
                elapsed.as_nanos() as f64 / per,
                allocations as f64 / per,
            );
        }
    }
}

/// Batches of keys, an equal share of each: owned versions present and
/// absent, a package, generations present, live streams.
fn availability(rng: &mut Rng) {
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    let count = 10_000;
    let written = (0..count)
        .map(|n| {
            let object = Object::with_id_owner_version_for_testing(
                ObjectID::new(object_id(n).0),
                SequenceNumber::from_u64(1),
                Owner::AddressOwner(SuiAddress::ZERO),
            );
            store::Written {
                id: object_id(n),
                version: 1,
                digest: Digest::new(object.digest().into_inner()),
                bytes: bcs::to_bytes(&object).unwrap(),
            }
        })
        .collect();
    store
        .write_through(store::Commit {
            written,
            ..store::Commit::default()
        })
        .unwrap();
    let (generations, _generations_dir) = generation_table();
    let stream = |n: u64| StreamId {
        id: object_id(n),
        initial_shared_version: 1,
    };
    for n in 0..count {
        generations.record(
            stream(n),
            1,
            1,
            Generation::Version {
                version: 2,
                digest: [0; 32],
            },
            [0; 32],
            true,
        );
    }
    let mut check = InputAvailability {
        store: Arc::new(store),
        generations,
    };
    println!("batch  ns/key  allocs/key");
    for batch in [100, 1000] {
        let keys: Vec<InputKey> = (0..batch)
            .map(|i| {
                let n = rng.below(count);
                match i % 5 {
                    0 => InputKey::ObjectVersion {
                        id: object_id(n),
                        version: 1,
                    },
                    1 => InputKey::ObjectVersion {
                        id: object_id(n),
                        version: 2,
                    },
                    2 => InputKey::Package(object_id(n)),
                    3 => InputKey::Generation {
                        stream: stream(n),
                        round: 1,
                        generation: 1,
                    },
                    _ => InputKey::StreamLive(stream(n)),
                }
            })
            .collect();
        let mut available = Vec::with_capacity(batch);
        let rounds = 200;
        let mut elapsed = Duration::ZERO;
        let mut allocations = 0;
        for _ in 0..rounds {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            check.check(&keys, &mut available);
            elapsed += start.elapsed();
            allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
            assert_eq!(
                available.iter().filter(|a| a.is_some()).count(),
                batch / 5 * 4
            );
        }
        let per = f64::from(rounds) * batch as f64;
        println!(
            "{batch:<5}  {:>6.1}  {:>10.1}",
            elapsed.as_nanos() as f64 / per,
            allocations as f64 / per,
        );
    }
}

fn main() {
    let epoch = Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        Digest::new([0; 32]),
        1000,
        4,
    ));
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    resolve(&epoch, &mut rng);
    availability(&mut rng);
}
