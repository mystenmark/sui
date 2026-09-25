# Phase 5 benchmark: validation through work queues

`cargo bench -p validator --bench validate [single|pool|grpc]`, over the
2,332 mainnet corpus transactions (mean 1,047 bytes). Knobs:
`POOL=inline|queue,tasks,rounds`, `GRPC=workers,tasks,batch`,
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

## Design as benchmarked

One validation processor thread (the pool API still takes a thread count);
one work item per request.

## Findings

1. **The handoff costs far more than the work.** Round trip for one item:
   ~22µs through the queue vs 2.9µs inline: two thread wakes (Mach
   semaphore into the processor; tokio's inject queue and unpark back). In
   process, 4 runtime workers validate 2.03M tx/s inline vs 0.77–0.98M
   through the queue.
2. **One item per request** (2986d3cc86): one handoff per request instead
   of per transaction; 16-tx requests went from ~1.0M to ~1.3M tx/s.
3. **At the gRPC level the stack dominates.** A no-op RPC tops out at ~180k
   req/s here. One-tx submits: 135–148k req/s through the processor, ~171k
   validating inline (temporary patch, not committed). 16-tx submits:
   0.96–1.28M tx/s.
4. **Several processor threads sharing one channel collapse.** Idle threads
   register and unregister on the crossbeam channel's waker mutex each time
   they park, and every push locks it to notify: with 12 threads, ~70% of
   samples in `__psynch_mutexwait`, 78k tx/s. A channel per thread fixed it
   (reverted, since there is one processor thread); also, for work this
   small more threads were slower than one. Revisit if pools grow.

## Measured but not adopted

- **Decode in the processor, handler forwards `Bytes`** (temporary patch,
  measured with a multi-thread pool). The PRD says handlers deserialize.
  With one processor thread, decode (1.2µs) would cap it near 800k tx/s.
- **Processor spins before parking** (20–100µs). In-process round trip
  22 → 10µs; no effect on a sequential gRPC caller (the gap between
  requests outlasts the spin); burns a core while idle.

## Open

- Blake2b at ~1.5 GB/s is the largest single cost; a faster implementation,
  or computing the digest later, would roughly halve decode.
