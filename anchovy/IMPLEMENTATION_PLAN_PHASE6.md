# Phase 6: signature verification as a processor; processors share threads

Requested after phase 5 (not in `PRD.md`): verify transaction signatures
in a processor, and let one thread run several processors.

## Overview

- `crates/workqueue`: queues and threads decouple. A queue is a pair: a
  `Queue` (senders, in handlers and upstream processors) and an `Inbox`
  (the receiving end). A `Worker` is one thread that runs any number of
  processors, each draining its own inbox. An inbox given to several
  workers is drained by all of them (the old pool), so processors and
  threads can be M:1, 1:N, or 1:1.
- `crates/validator`: a two-stage pipeline on one worker thread.
  Validation checks each request's transactions and computes their
  digests, then passes the request on to signature verification, which
  replies to the handler.

## Decision: validation grows from `TransactionData` to `SenderSignedData`

Phase 5 scoped the validation processor to `TransactionData::validity_check`.
Signature verification needs what the reference checks before it: the
checks its deserializer makes (intent, signature encodings), the enabled
signature schemes, no system transactions, the size limit, and then
`TransactionData::validity_check`: `SenderSignedData::validity_check`
(`validation::sender_signed::validity_check`). For a transaction that fails
several checks, the reference reports the first in that order, so the
validation processor now runs the whole of it, and the verifier runs only
signature verification. Together they give `validation::check`'s verdict,
which is differential-tested against the reference.

## Goals

1. A thread runs several processors; a processor's items stay on the
   threads that drain its inbox. No processor waits on another thread.
2. Handlers still only decode, enqueue and await one reply per request.
3. The pipeline's verdict for a request is `validation::check`'s for its
   first failing transaction, or success with the transactions and their
   digests.
4. A full queue anywhere refuses the request at once (`resource_exhausted`).
5. Accepting an Ed25519/Secp256k1 transaction allocates nothing on the
   processor thread once warm.

## Non-goals

- Address aliases (they are object state): verification uses none, as for
  an address without an alias object.
- JWK updates: the epoch's JWKs are fixed at startup (none from the command
  line), so zkLogin transactions fail until JWKs come with state.
- The reference's cache of verified signatures.
- More than one thread by default; consensus submission.

## Design

### `workqueue`

- `queue(capacity) -> (Queue<W>, Inbox<W>)`: a bounded channel.
  `Queue::try_push` as before (`Full`, `Closed`). `Inbox` clones share the
  channel; the queue closes when every inbox is dropped. An inbox no worker
  drains fills and refuses.
- `Worker::new(name).run(inbox, make).run(inbox2, make2).spawn()`: one
  thread named `name`. Each `make` builds its processor on that thread, so
  processor state needs no `Send`. The thread waits on all its inboxes and a
  stop channel (`crossbeam_channel::Select::ready`, which picks among ready
  inboxes at random, so none starves), takes an item with `try_recv`, and
  runs its processor under `catch_unwind` as before. An inbox whose queues
  are all dropped leaves the set. Dropping the `WorkerHandle` stops the
  thread after the item in hand and joins it; queued items are dropped.
- Processors on one thread run one item at a time: a slow item delays the
  thread's other processors. That is the trade for not handing off.
- A processor may push to another processor's queue, including one on its
  own thread. It must not block on it (`try_push` only), or a thread could
  wait on itself.

### Pipeline

- `ValidateTransactions { transactions: Vec<Message<Transaction<DigestPending>>>, reply }`
  → `TransactionValidator`: `sender_signed::validity_check` per transaction
  (arena reset per transaction), then `Message::with_digests`, then
  `VerifySignatures { transactions: Vec<Message<Transaction<DigestReady>>>, reply }`
  pushed to the verifier's queue. A full queue replies `Overloaded`.
- Order within a request: the reference validates a transaction and checks
  its signatures before the next (`handle_submit_transaction_inner`), so a
  bad signature on the first beats an invalid second. Validation stops at
  the first invalid transaction and forwards the ones before it with that
  error (`VerifySignatures::then`); the verifier reports a signature failure
  among them first, else that error.
- `SignatureVerifier`: per transaction, `deserialization_checks` (to parse
  the signatures again into its own arena; validation's arena is reset per
  transaction) then `verify::verify_signatures`, and replies.
- Reply: `Result<Validated, Rejected>`, `Rejected` being `Invalid(validation::Error)`
  (`invalid_argument` with the kind, as now), `Overloaded`
  (`resource_exhausted`) or `ShuttingDown` (`unavailable`).
- `EpochState` gains the signature `Verifier` (protocol config, chain,
  JWKs), built once per epoch. `validation::verify` re-exports the JWK
  types.
- `Processors::start` builds both queues and one worker running both
  processors.

### Verified-signature cache

The reference (`sui-core` `SignatureVerifier::verify_tx`) skips verifying a
transaction it has verified before: an LRU of 100,000
`full_message_digest_with_alias_versions`, a hash of the whole
`SenderSignedData` (intent, data and signatures) and the signers' alias
versions. Only successes are cached. The cache belongs to the epoch's
store, so a new epoch starts empty. (A second cache, of verified zkLogin
proof inputs, is not reproduced here.)

The same, in the verifier processor (revised after the security review,
below):

- Key: Blake2b-256 over a domain tag, the epoch, the transaction digest,
  the intent, and each signature's bytes, length-prefixed, in order. The
  transaction digest commits to the data's bytes, from which the rest of
  the data is parsed, so the key covers every byte verification reads, as
  the reference's does; a transaction resubmitted with any other
  signatures misses.
- The cache takes a whole `Message<Transaction<DigestReady>>`, whose view
  cannot be changed and whose digest was computed from its own bytes, and
  hands that same view to the verification closure: the key and what is
  verified cannot differ. `SignatureCache` is crate-private.
- Only a verification that succeeded is cached; the verdict is otherwise
  unchanged.
- Everything else verification depends on is the epoch's: its number
  (zkLogin's max epoch), protocol config and JWKs. A request carries the
  `Arc<EpochState>` it was validated in; the verifier verifies in it, and
  the cache empties when handed another, besides having the epoch number
  in the key.
- Only `TransactionValidator` makes a `VerifySignatures` (private fields):
  `verify_signatures` does not repeat validation (it passes system
  transactions, for one), so neither verifying nor caching may happen
  without it.
- Aliases would change the verdict and are not in the key: there are none
  yet (verification passes `&[]`), and the cached closure takes none. When
  they arrive the key must include the signers' alias versions, as the
  reference's does.
- Bounded, and no allocation once built: two generations of 100,000.
  Inserts go to the current one; when it is full it becomes the previous
  one and the old previous is cleared (its table kept). Hits insert
  nothing, so every entry outlives at least 100,000 later insertions (the
  reference, which does not refresh on hits either, keeps 100,000), and
  resubmitting cached transactions evicts nothing. About 8.5 MiB.
- The table is `hashbrown` with the `containers::DigestHasher`: a per-map
  secret seed, so that keys ground to collide in the table's low bits do
  not degrade it.
- Owned by the verifier processor, on its thread: no lock.

### Security review

Three independent reviews (key soundness; lifecycle and pipeline; denial
of service) found no way, through the RPC path, for the cache to accept a
transaction without a valid signature or refuse a valid one. Fixed from
their findings: a view could be edited, or measure-parsed with a zero
digest, and still hit another transaction's entry (in-process code only);
anyone could build a `VerifySignatures`, and the verifier alone accepts a
system transaction; validation and verification held separate epochs;
hits could evict other entries, and the effective capacity was
50,000–100,000. Recorded, not changed:

- Aliases: the reference enables address aliases from protocol version
  116; verifying with none differs from it for any address that has
  aliases, cache or not. Needs object state.
- JWKs: the reference adds them within an epoch. If JWKs are ever updated
  in place rather than by replacing the `EpochState`, updates must only
  add, or cached successes go stale.
- An invalid transaction after a valid one in a request is reported as
  `Overloaded` when the signature queue is full: the request was not
  judged, and a retry gets its verdict.
- `[t, t]` is accepted; the reference refuses the repeat. Request-level
  checks are a phase 5 non-goal.
- A hit is visible in the response time, as in the reference.

## Testing

- `workqueue`: two processors on one thread run there and both drain; a
  processor feeding another on its own thread; one inbox drained by two
  workers, each with its own state; backpressure; a panicking item leaves
  the thread and its other processors running; an undrained inbox fills,
  and the queue closes once the inboxes are dropped; dropping the handle
  joins the thread.
- Pipeline: every `sender_signed` and `verify` validity vector, and every
  transaction of the checked-in mainnet checkpoint, through the processors,
  against `validation::check` called directly, with the vectors' JWKs.
- End to end over gRPC: the same vectors get `invalid_argument` with the
  right kind or `unimplemented`; malformed requests and a full queue as
  before. `tools/validator-client`: a transfer signed by its sender passes,
  one with a corrupted signature gets `InvalidSignature`.
- Allocations: a warm validator and verifier accept Ed25519/Secp256k1
  mainnet transactions without allocating.
- Benchmark: per-transaction cost of each stage, and the pipeline in
  process and over gRPC.

## Steps

1. `workqueue`: `queue`/`Inbox`, `Worker` with several processors; `Pool`
   removed.
2. `EpochState` with the verifier; `Rejected`.
3. Validation over `SenderSignedData`, forwarding; the verifier processor;
   one worker for both.
4. Tests: pipeline, gRPC, allocations, validator-client.
5. Benchmark and notes.
6. The verified-signature cache, its tests, and a security review of it by
   independent agents.
