# Phase 5 benchmark: validation through work queues

`cargo bench -p validator --bench validate [single|pool|grpc]`, over the
2,332 mainnet corpus transactions (mean 1,047 bytes). Knobs:
`POOL=threads,tasks,rounds`, `GRPC=workers,threads,tasks,batch`,
`CONNECTIONS`, `CLIENT_WORKERS`.

Machine: Apple M4 Max VM, 16 vCPUs, macOS. Thread wakeups go through Mach
semaphores and are slower than Linux futexes; the gRPC client shares the
machine; plaintext, no TLS.

## Costs per transaction, one thread

| step | ns | allocs |
|---|---|---|
| `validity_check` | 131 | 0 |
| decode: copy + parse + digest, and drop | 1,200 | 2 |
| &nbsp;&nbsp;of which Blake2b of `TransactionData` | 690 | 0 |
| &nbsp;&nbsp;of which the copy | 50 | 1 |
| oneshot create/send/receive | 20 | 1 |

The check is a tenth of the decode, and the digest is over half of it.

## Findings

1. **One shared MPMC channel collapsed with more threads.** Idle threads
   register and unregister on the channel's waker mutex every time they park,
   and every push locks it to notify. With 12 threads, ~70% of samples were
   in `__psynch_mutexwait` under crossbeam's `SyncWaker`: 78k tx/s. Fixed
   with a channel per thread, round-robin with fallback (2040745623): 700k.
2. **The handoff costs far more than the work.** Round trip for one item:
   ~22µs through the pool vs 2.7µs inline, two thread wakes (Mach semaphore
   into the processor; tokio's inject queue and unpark back). In process,
   4 runtime workers validate 2.08M tx/s inline vs 0.95M through the pool.
   More processor threads lower throughput (0.95M at 1, 0.48M at 4): each
   thread parks between items and each item then pays a wake.
3. **One item per request** (2986d3cc86): 16-tx requests went from
   0.96–1.12M to 1.28–1.33M tx/s (inline: 1.46M).
4. **At the gRPC level the stack dominates.** A no-op RPC tops out at ~180k
   req/s here. One-tx submits: 130–147k req/s through the pool, ~171k
   validating inline (temporary patch, not committed).

Before/after the fixes (gRPC, submit): 1 tx, 8 processor threads 58k → 147k
req/s; 16 txs, 8 threads 119k → 1.31M tx/s.

## Measured but not adopted

- **Decode in the processor, handler forwards `Bytes`** (temporary patch).
  With 4 runtime workers: 16-tx 0.89M → 1.30M tx/s (beats inline's 1.07M,
  since decode then runs on more cores); 1-tx 123k → 149k req/s. The PRD
  says handlers deserialize, so this is a design decision.
- **Processors spin before parking** (20–100µs). In-process round trip
  22 → 10µs, throughput +35%; no effect on a sequential gRPC caller (the gap
  between requests outlasts the spin); burns a core per idle thread.

## Open

- `--validation-threads` defaults to half the cores; for this processor 1–2
  threads are faster than more.
- Blake2b at ~1.5 GB/s is the largest single cost; a faster implementation,
  or computing the digest later, would roughly halve decode.
