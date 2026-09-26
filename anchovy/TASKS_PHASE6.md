# Phase 6 progress: signature verification; processors share threads

Plan: `IMPLEMENTATION_PLAN_PHASE6.md`. Branch `mlogan-phase6`, to be merged
into `anchovy-main`.

## Status: all steps done

## Done

1. `workqueue`: `queue()` returns a `Queue` and an `Inbox`; a `Worker` thread
   runs several processors (`Select::ready` over their inboxes, `try_recv`,
   processors in `RefCell`s); an inbox given to several workers is shared.
   `Pool` removed.
2. `EpochState` holds the signature `Verifier`; `validation::verify`
   re-exports the JWK types. `Rejected`: `Invalid`, `Overloaded`,
   `ShuttingDown`.
3. `TransactionValidator` runs `sender_signed::validity_check`, hashes, and
   forwards to `SignatureVerifier`, which re-parses the signatures and
   verifies them; one worker runs both. Per-transaction order as the
   reference (see the plan).
4. Tests: signed vectors (every scheme) and the mainnet checkpoint, singly
   and in requests of two and three, against `validation::check`; the
   ordering case; gRPC verdicts, malformed requests, a full first queue, a
   full queue between the processors; warm processors accept plain
   signatures without allocating; `validator-client` sees a corrupted
   signature refused.
5. Benchmark (below).

## Measurements

`cargo bench -p validator --bench validate`, 1,681 mainnet user
transactions (system transactions excluded), M4 Max VM:

| step, one thread | per tx |
|---|---|
| `SenderSignedData` validity check | 215 ns |
| decode (handler) | 616 ns |
| Blake2b digest | 793 ns |
| signatures (1,679 Ed25519, 1 Secp256k1, 1 zkLogin) | 28.2 µs |
| both processors | 30.0 µs |

Signature verification is 94% of the processor thread's work, and the
thread caps throughput: 25k tx/s in process (inline on 4 runtime workers:
129k), 22.8k one-transaction requests/s over gRPC (a no-op RPC: 178k).

## Next

- The one thread is the limit. More verifier threads need only the
  verification inbox given to more workers.
- Ed25519 batch verification, and the reference's cache of verified
  signatures.
- Aliases and JWK updates come with state.
