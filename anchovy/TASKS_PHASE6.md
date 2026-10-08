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

6. Verified-signature cache (plan: "Verified-signature cache"), reviewed
   by three independent agents and revised (plan: "Security review").
   Tests: resubmissions hit with the same verdicts; every single-bit change
   of a cached transaction is judged as without the cache and never hits;
   the same data with other signatures is refused; failures are not
   cached; another epoch empties it; hits evict nothing and an entry
   outlives a generation; the key distinguishes signature order, count,
   boundaries and epoch; verification uses the validation epoch; a hit
   allocates nothing.
7. `messages`: `TransactionData` and `SenderSignedData` fields private
   behind getters; `Measure` crate-private and `Alloc` sealed. No view
   outside the crate can disagree with its bytes or digest
   (`compile_fail` doctests).
8. Checks in the types (plan: "Checks in the types"). `messages`:
   `TxState`, `ParseState` (sealed), `HasDigest`, `Attested`;
   `Message::relabel`/`relabel_all` with a witness, in place; `Wire` split
   into `Wire` and `Parse`. `workqueue`: processors declare `Input` and
   `Output`, the worker routes outputs to a `Sink` (a `Queue` refuses
   through `Refuse`). `validator::checks`: `Valid`, `Verified`, their
   witnesses, `validate`, `SignatureChecks` (the cache moved here and takes
   only `Valid`). `Request<S>` carries the epoch through
   `TransactionValidator` → `SignatureVerifier` → `answer`; processors no
   longer hold an epoch. `compile_fail` doctests: parsing into a checked
   state, implementing `ParseState`, forging a witness, giving a `Valid`
   transaction where a `Verified` one is needed, wiring a mismatched
   inbox. Their error codes are checked only by nightly rustdoc
   (`cargo +nightly test --doc`); stable checks only that they fail.

## Measurements

`cargo bench -p validator --bench validate`, 1,681 mainnet user
transactions (system transactions excluded), M4 Max VM:

| step, one thread | per tx |
|---|---|
| `SenderSignedData` validity check | 215 ns |
| decode (handler) | 616 ns |
| Blake2b digest | 793 ns |
| signatures (1,679 Ed25519, 1 Secp256k1, 1 zkLogin) | 28.2 µs |
| both processors | 30.0 µs (31.0 µs with a cold signature cache) |
| both processors, signatures cached | 2.1 µs |

Signature verification is 94% of the processor thread's work, and the
thread caps throughput: 25k tx/s in process (inline on 4 runtime workers:
129k), 22.8k one-transaction requests/s over gRPC (a no-op RPC: 178k).

## Next

- The one thread is the limit. More verifier threads need only the
  verification inbox given to more workers.
- Ed25519 batch verification, and the reference's cache of verified
  signatures.
- Aliases and JWK updates come with state; see the plan's "Security
  review" for what each must preserve in the signature cache.
