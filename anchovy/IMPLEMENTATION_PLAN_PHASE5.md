# Phase 5: work queues and processors

Requirements are in `PRD.md`, Phase 5: work is done by processors on
dedicated threads, fed by work queues; the tokio runtime only serves RPCs.
Handlers deserialize, enqueue, and await the result on a
`tokio::sync::oneshot`. The first processor validates transactions, doing
*only* what `TransactionData::validity_check` does.

## Overview

- `crates/workqueue`: the architecture's two pieces, independent of what
  flows through them. A bounded queue of work items, and a pool of
  dedicated threads each running a processor over it.
- `arena::Bump::reset`, so a processor thread reuses one arena for every
  item instead of allocating one per item.
- `messages`: a `Transaction` wire type, the envelope the submit RPC
  carries, so decoding counts its depth as the reference does.
- `crates/validator`: epoch state, the transaction validation processor,
  and `SubmitTransaction` decoding and enqueueing transactions.

## Goals

1. No transaction work runs on a tokio thread: handlers decode, enqueue and
   await; processors run on threads they own.
2. A full queue is refused at once (backpressure), not queued unbounded.
3. The validation processor returns exactly
   `validation::transaction_data::validity_check`'s verdict, and accepting
   a transaction allocates nothing on the heap once the processor is warm.
4. Shutdown drains nothing new, finishes items in hand, and joins threads.

## Non-goals

- Signature verification and `SenderSignedData` checks in the processor:
  the PRD scopes this processor to `TransactionData::validity_check`.
  `validation::check` is ready for when that changes.
- Consensus submission: a transaction that validates has nowhere to go
  yet, and `SubmitTransaction` says so (`unimplemented`).
- Request-level checks (repeated digests, soft-bundle prices, batch byte
  limits), reference-compatible error encoding (`SuiError` in the status
  details), and epoch changes: later, with consensus.

## Design

### `workqueue`

`Queue<W>`: a bounded multi-producer, multi-consumer channel
(`crossbeam-channel`). `try_push` fails immediately when full.

`Pool`: `Pool::spawn(name, threads, queue, make_processor)` starts
`threads` OS threads named `name-N`; each builds its own processor state
with `make_processor` (so per-thread state, like an arena, needs no
locking) and runs `Processor::process(&mut self, item)` for each item it
receives. Dropping the queue's last sender ends the threads after the
items already queued; dropping the pool joins them. A panic in a processor
is caught per item, so one bad item does not take the thread down; its
reply sender is dropped, which the handler sees as an internal error.

This is the component-system shape the PRD describes: data (work items)
flows through queues; behaviour lives in processors that own their state.

### Epoch state

`EpochState`: protocol config, epoch, chain identifier, reference gas
price, committee size — what `validation::Context` borrows. Shared as
`Arc`, set at startup from the command line (chain, protocol version,
epoch); swapping it at epoch change comes with reconfiguration.

### Transaction validation processor

Work item: a decoded `Message<Transaction>` (owning its wire bytes and
arena) and a `oneshot::Sender<Result<(), validation::Error>>`. The
processor owns a `Bump`, runs `TransactionData::validity_check` against
the epoch state, sends the result, and resets the arena.

### `SubmitTransaction`

As the reference: the submit type must be known; a ping carries no
transactions and a submission at least one. Each transaction is decoded
as `Transaction` in the handler (a failure fails the request with
`invalid_argument`, as the reference fails it with
`TransactionDeserializationError`), then all are enqueued (a full queue
fails the request with `resource_exhausted`) and awaited. A validation
failure fails the request (the reference fails the request on a
`validity_check` error); if all pass, the response is `unimplemented`:
consensus submission. Pings stay `todo!()`.

## Testing

- `workqueue`: items processed on the pool's threads (thread names), each
  thread's state private, backpressure when full, drain and join on
  shutdown, a panicking item not killing its thread.
- Processor: the verdicts of the `tx_data` validity vectors at one version
  and chain, through the processor, equal the direct call's; an
  allocation count shows nothing allocated per accepted transaction after
  warm-up.
- End to end: the in-process server receives transactions over gRPC:
  malformed bytes, invalid transactions (the vectors' failing cases) and
  valid ones get `invalid_argument` with the right error, and
  `unimplemented`, respectively; the validation ran on a processor thread.
  `tools/validator-client` submits a real transaction with sui's client.

## Steps

1. `arena::Bump::reset`.
2. `workqueue` crate.
3. `messages::transaction::Transaction` wire type.
4. Epoch state and the validation processor.
5. `SubmitTransaction` through the processor; command-line epoch state.
6. End-to-end tests and the reference client run.
