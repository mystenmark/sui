// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validity checking through the work queue and processors, layer by
//! layer, over the mainnet corpus (`scripts/fetch-mainnet.sh`): the check
//! alone, the handler's decode, the processor, the queue and pool, and the
//! whole gRPC path. Run with `cargo bench -p validator`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use containers::Bump;
use messages::Message;
use messages::base::Digest;
use messages::checkpoint::CheckpointData;
use messages::transaction::{DigestPending, DigestReady, Transaction, TransactionKind};
use protocol_config::{Chain, ProtocolVersion};
use tokio::sync::oneshot;
use tonic::transport::Channel;
use tonic::transport::server::TcpIncoming;
use validator::Validator;
use validator::epoch::EpochState;
use validator::processors::{
    Processors, SignatureVerifier, TransactionValidator, ValidateTransactions,
};
use validator::proto::{RawSubmitTxRequest, RawValidatorHealthRequest, SubmitTxType};
use validator::service::validator_client::ValidatorClient;
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

const MAINNET_CHAIN_ID: &str = "35834a8ac17ca48fb14ac8f99c17c98747e95dd07294ae41a46b382246a4499b";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The corpus's user transactions as `SubmitTransaction` carries them, and
/// the epoch of the first checkpoint.
fn load() -> (Vec<Bytes>, u64) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![manifest.join("../messages/tests/data/mainnet-325300367.chk")];
    if let Ok(entries) = std::fs::read_dir(manifest.join("../../corpus/mainnet")) {
        paths = entries.map(|e| e.unwrap().path()).collect();
        paths.retain(|p| p.extension().is_some_and(|e| e == "chk"));
        paths.sort();
    }
    let mut transactions = vec![];
    let mut epoch = None;
    for path in paths {
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
        let view = checkpoint.get();
        epoch.get_or_insert(view.checkpoint_summary.data.epoch);
        // Users submit only programmable transactions; checkpoints also hold
        // system ones.
        for tx in view.transactions {
            if matches!(
                tx.transaction.data.kind,
                TransactionKind::ProgrammableTransaction(_)
            ) {
                transactions.push(Bytes::copy_from_slice(tx.transaction.bytes));
            }
        }
    }
    (transactions, epoch.unwrap())
}

fn epoch_state(epoch: u64) -> Arc<EpochState> {
    Arc::new(EpochState::new(
        Chain::Mainnet,
        ProtocolVersion::MAX.as_u64(),
        epoch,
        Digest::new(unhex(MAINNET_CHAIN_ID).try_into().unwrap()),
        1,
        100,
        // No JWKs: zkLogin transactions fail verification early.
        [],
    ))
}

fn decode(bytes: &Bytes) -> Message<Transaction<'static, DigestPending>> {
    Message::<Transaction<DigestPending>>::parse(bytes.to_vec())
        .map_err(|(e, _)| e)
        .unwrap()
}

/// Per-transaction time of the best of `rounds` runs of `f` over the
/// corpus, and allocations per transaction.
fn per_tx(txs: &[Bytes], rounds: usize, mut f: impl FnMut(&[Bytes])) -> (Duration, f64) {
    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..rounds {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        f(txs);
        best = best.min(start.elapsed());
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    }
    (
        best / txs.len() as u32,
        allocations as f64 / txs.len() as f64,
    )
}

fn row(name: &str, (time, allocations): (Duration, f64)) {
    let rate = 1.0 / time.as_secs_f64();
    println!(
        "  {name:<44} {:>7} ns/tx {:>11.0} tx/s {allocations:>6.1} allocs/tx",
        time.as_nanos(),
        rate
    );
}

/// The layers a transaction passes through, each on one thread.
// A flat list of rows, one measurement each.
#[allow(clippy::too_many_lines)]
fn single_thread(txs: &[Bytes], epoch: &Arc<EpochState>) {
    println!("single thread, per transaction:");
    let decoded: Vec<_> = txs.iter().map(decode).collect();
    let bump = Bump::with_capacity(64 * 1024);
    let mut bump = bump;
    let mut verdicts = std::collections::BTreeMap::<String, usize>::new();
    for m in &decoded {
        bump.reset();
        let verdict = validation::check(&m.get().0, &epoch.context(), &epoch.verifier, &[], &bump)
            .map_or_else(|e| format!("{:?}", e.kind), |_| "ok".to_owned());
        *verdicts.entry(verdict).or_default() += 1;
    }
    println!(
        "  {} transactions, mean {} bytes; validation::check: {verdicts:?}",
        txs.len(),
        txs.iter().map(Bytes::len).sum::<usize>() / txs.len()
    );

    row(
        "SenderSignedData validity_check",
        per_tx(txs, 50, |_| {
            for m in &decoded {
                bump.reset();
                let _ = black_box(validation::sender_signed::validity_check(
                    &m.get().0,
                    &epoch.context(),
                    &bump,
                ));
            }
        }),
    );
    row(
        "  of which: TransactionData validity_check",
        per_tx(txs, 50, |_| {
            for m in &decoded {
                bump.reset();
                let _ = black_box(validation::transaction_data::validity_check(
                    &m.get().0.data,
                    &epoch.context(),
                    &bump,
                ));
            }
        }),
    );
    row(
        "signatures: deserialization_checks + verify",
        per_tx(txs, 5, |_| {
            for m in &decoded {
                bump.reset();
                let signed = &m.get().0;
                let Ok((signatures, _)) =
                    validation::sender_signed::deserialization_checks(signed, &bump)
                else {
                    continue;
                };
                let _ = black_box(validation::verify::verify_signatures(
                    signed,
                    signatures,
                    epoch.epoch,
                    &epoch.verifier,
                    &[],
                    &bump,
                ));
            }
        }),
    );
    for (flag, scheme) in [
        (0, "Ed25519"),
        (1, "Secp256k1"),
        (2, "Secp256r1"),
        (3, "multisig"),
        (5, "zkLogin"),
        (6, "passkey"),
    ] {
        let group: Vec<_> = decoded
            .iter()
            .filter(|m| m.get().0.tx_signatures.first().and_then(|s| s.0.first()) == Some(&flag))
            .collect();
        if group.is_empty() {
            continue;
        }
        let (time, allocations) = per_tx(&txs[..group.len()], 5, |_| {
            for m in &group {
                bump.reset();
                let signed = &m.get().0;
                let Ok((signatures, _)) =
                    validation::sender_signed::deserialization_checks(signed, &bump)
                else {
                    continue;
                };
                let _ = black_box(validation::verify::verify_signatures(
                    signed,
                    signatures,
                    epoch.epoch,
                    &epoch.verifier,
                    &[],
                    &bump,
                ));
            }
        });
        row(
            &format!("  {scheme} first ({} transactions)", group.len()),
            (time, allocations),
        );
    }
    row(
        "decode (copy + parse), then drop",
        per_tx(txs, 50, |txs| {
            for bytes in txs {
                black_box(decode(bytes));
            }
        }),
    );
    row(
        "  of which: copy to a Vec, then drop",
        per_tx(txs, 50, |txs| {
            for bytes in txs {
                black_box(bytes.to_vec());
            }
        }),
    );
    row(
        "Blake2b of TransactionData",
        per_tx(txs, 50, |_| {
            for m in &decoded {
                black_box(Digest::of("TransactionData", m.get().0.data.bytes));
            }
        }),
    );
    row(
        "  Message::with_digests over a Vec",
        with_digest_per_tx(txs, 50, Message::with_digests),
    );
    row(
        "  into_iter().map(Message::with_digest).collect()",
        with_digest_per_tx(txs, 50, |v| {
            v.into_iter().map(Message::with_digest).collect()
        }),
    );
    row(
        "decode, two-pass (parse_exact)",
        per_tx(txs, 20, |txs| {
            for bytes in txs {
                black_box(
                    Message::<Transaction<DigestPending>>::parse_exact(bytes.to_vec()).unwrap(),
                );
            }
        }),
    );
    row(
        "oneshot: create, send, receive",
        per_tx(txs, 50, |txs| {
            for _ in txs {
                let (tx, rx) = oneshot::channel::<Result<(), validation::Error>>();
                let _ = tx.send(Ok(()));
                black_box(rx.blocking_recv().unwrap().is_ok());
            }
        }),
    );
    let (signatures, verification) = workqueue::queue(1);
    let mut validator = TransactionValidator::new(epoch.clone(), signatures);
    let mut verifier = SignatureVerifier::new(epoch.clone());
    row(
        "decode + both processors (+ digest)",
        per_tx(txs, 5, |txs| {
            for bytes in txs {
                let (reply, verdict) = oneshot::channel();
                validator.process(ValidateTransactions {
                    transactions: vec![decode(bytes)],
                    reply,
                });
                if let Some(next) = verification.try_pop() {
                    verifier.process(next);
                }
                black_box(verdict.blocking_recv().unwrap().is_ok());
            }
        }),
    );
}

/// Converting the corpus, as one `Vec`, to computed digests with `convert`:
/// the time and allocations of the conversion alone, decoding excluded.
fn with_digest_per_tx(
    txs: &[Bytes],
    rounds: usize,
    convert: impl Fn(
        Vec<Message<Transaction<'static, DigestPending>>>,
    ) -> Vec<Message<Transaction<'static, DigestReady>>>,
) -> (Duration, f64) {
    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..rounds {
        let pending: Vec<_> = txs.iter().map(decode).collect();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        let ready = convert(pending);
        best = best.min(start.elapsed());
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        black_box(ready);
    }
    (
        best / txs.len() as u32,
        allocations as f64 / txs.len() as f64,
    )
}

/// The processors' work on the calling thread, in its own arena: decode,
/// validate and verify, then hash.
fn validate_inline(bytes: &Bytes, epoch: &EpochState) -> bool {
    thread_local! {
        static BUMP: std::cell::RefCell<Bump> = std::cell::RefCell::new(Bump::with_capacity(64 * 1024));
    }
    let transaction = decode(bytes);
    let ok = BUMP.with_borrow_mut(|bump| {
        bump.reset();
        validation::check(
            &transaction.get().0,
            &epoch.context(),
            &epoch.verifier,
            &[],
            bump,
        )
        .is_ok()
    });
    ok && black_box(Message::with_digests(vec![transaction])).len() == 1
}

/// Tasks on a `workers`-thread runtime, each in a closed loop (decode,
/// push, await) over its share of `rounds` passes of the corpus, against
/// the processor, or validating inline. Returns transactions per second and the mean
/// round trip.
fn pool(
    txs: &Arc<Vec<Bytes>>,
    epoch: &Arc<EpochState>,
    inline: bool,
    workers: usize,
    tasks: usize,
    rounds: usize,
) -> (f64, Duration) {
    let processors = Processors::start(epoch, 1 << 16);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .build()
        .unwrap();
    let total = txs.len() * rounds;
    let elapsed = runtime.block_on(async {
        let start = Instant::now();
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let queue = processors.transactions.clone();
                let txs = txs.clone();
                let epoch = epoch.clone();
                tokio::spawn(async move {
                    let mut i = t;
                    while i < total {
                        if inline {
                            black_box(validate_inline(&txs[i % txs.len()], &epoch));
                            i += tasks;
                            // Let other tasks run, as a handler would.
                            tokio::task::yield_now().await;
                            continue;
                        }
                        let (reply, verdict) = oneshot::channel();
                        let transaction = decode(&txs[i % txs.len()]);
                        if queue
                            .try_push(ValidateTransactions {
                                transactions: vec![transaction],
                                reply,
                            })
                            .is_err()
                        {
                            panic!("queue full");
                        }
                        black_box(verdict.await.unwrap().is_ok());
                        i += tasks;
                    }
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        start.elapsed()
    });
    let rate = total as f64 / elapsed.as_secs_f64();
    let latency = elapsed * tasks as u32 / total as u32;
    (rate, latency)
}

fn pools(txs: &[Bytes], epoch: &Arc<EpochState>) {
    let txs = Arc::new(txs.to_vec());
    println!("\nqueue + processor: closed-loop tasks on a 4-worker runtime (decode, push, await):");
    println!(
        "  {:<10} {:>6} {:>11} {:>11}",
        "", "tasks", "tx/s", "round trip"
    );
    let mut configs = vec![
        (true, 1, 5),
        (true, 64, 40),
        (false, 1, 5),
        (false, 64, 20),
        (false, 256, 20),
    ];
    // POOL=inline|queue,tasks,rounds runs one configuration, for profiling.
    if let Ok(one) = std::env::var("POOL") {
        let n: Vec<&str> = one.split(',').collect();
        configs = vec![(
            n[0] == "inline",
            n[1].parse().unwrap(),
            n[2].parse().unwrap(),
        )];
    }
    for (inline, tasks, rounds) in configs {
        let (rate, latency) = pool(&txs, epoch, inline, 4, tasks, rounds);
        let name = if inline { "inline" } else { "queue" };
        println!("  {name:<10} {tasks:>6} {rate:>11.0} {latency:>11.1?}");
    }
}

/// Stops a benchmark server: its runtime, then its processors.
struct Guard(Option<tokio::runtime::Runtime>, Option<Processors>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.take().unwrap().shutdown_background();
        drop(self.1.take());
    }
}

/// An in-process server, plaintext, on its own runtime.
fn server(epoch: &Arc<EpochState>, workers: usize) -> (SocketAddr, impl Drop) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();
    let processors = Processors::start(epoch, 1 << 16);
    let service = Validator::new(epoch.clone(), processors.transactions.clone()).into_service();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    runtime.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
    });
    (addr, Guard(Some(runtime), Some(processors)))
}

/// Requests per second from `tasks` concurrent callers over `connections`
/// connections, each request carrying `batch` transactions (or a health
/// check when `batch` is 0).
fn grpc(
    txs: &Arc<Vec<Bytes>>,
    addr: SocketAddr,
    connections: usize,
    tasks: usize,
    batch: usize,
    duration: Duration,
) -> (f64, Duration) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::env::var("CLIENT_WORKERS").map_or(6, |c| c.parse().unwrap()))
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut clients = vec![];
        for _ in 0..connections {
            let channel = Channel::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            clients.push(ValidatorClient::new(channel));
        }
        let done = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let mut client = clients[t % connections].clone();
                let txs = txs.clone();
                let done = done.clone();
                tokio::spawn(async move {
                    let mut i = t * 7919;
                    while start.elapsed() < duration {
                        if batch == 0 {
                            client
                                .validator_health(RawValidatorHealthRequest {})
                                .await
                                .unwrap();
                        } else {
                            let transactions = (0..batch)
                                .map(|k| txs[(i + k) % txs.len()].clone())
                                .collect();
                            let status = client
                                .submit_transaction(RawSubmitTxRequest {
                                    transactions,
                                    submit_type: SubmitTxType::Default as i32,
                                })
                                .await
                                .unwrap_err();
                            black_box(status);
                            i += batch;
                        }
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        let requests = done.load(Ordering::Relaxed);
        (
            requests as f64 / elapsed.as_secs_f64(),
            elapsed * tasks as u32 / requests as u32,
        )
    })
}

fn end_to_end(txs: &[Bytes], epoch: &Arc<EpochState>) {
    let txs = Arc::new(txs.to_vec());
    let duration = Duration::from_secs(3);
    println!("\ngRPC, plaintext, in process (client: CLIENT_WORKERS=6 workers, CONNECTIONS=16):");
    println!(
        "  {:<16} {:>8} {:>6} {:>11} {:>11} {:>11}",
        "request", "workers", "tasks", "requests/s", "tx/s", "round trip"
    );
    let mut configs = vec![
        ("health", 4, 256, 0),
        ("health", 8, 256, 0),
        ("submit, 1 tx", 4, 1, 1),
        ("submit, 1 tx", 4, 256, 1),
        ("submit, 1 tx", 8, 256, 1),
        ("submit, 16 txs", 4, 64, 16),
        ("submit, 16 txs", 8, 64, 16),
    ];
    // GRPC=workers,tasks,batch runs one configuration.
    if let Ok(one) = std::env::var("GRPC") {
        let n: Vec<usize> = one.split(',').map(|n| n.parse().unwrap()).collect();
        configs = vec![("custom", n[0], n[1], n[2])];
    }
    let connections = std::env::var("CONNECTIONS").map_or(16, |c| c.parse().unwrap());
    for (name, workers, tasks, batch) in configs {
        let (addr, guard) = server(epoch, workers);
        let (rate, latency) = grpc(&txs, addr, connections, tasks, batch, duration);
        drop(guard);
        println!(
            "  {name:<16} {workers:>8} {tasks:>6} {rate:>11.0} {:>11.0} {latency:>11.1?}",
            rate * batch.max(1) as f64
        );
    }
}

fn main() {
    let (txs, epoch) = load();
    let epoch = epoch_state(epoch);
    let only = std::env::args().nth(1).filter(|a| !a.starts_with('-'));
    let run = |name: &str| only.as_deref().is_none_or(|o| o == name);
    if run("single") {
        single_thread(&txs, &epoch);
    }
    if run("pool") {
        pools(&txs, &epoch);
    }
    if run("grpc") {
        end_to_end(&txs, &epoch);
    }
}
