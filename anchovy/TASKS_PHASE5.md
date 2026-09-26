# Phase 5 progress: work queues and processors

Plan: `IMPLEMENTATION_PLAN_PHASE5.md`. Branch `mlogan-phase5`, to be merged
into `anchovy-main`.

## Status: all steps done; PR next

## Done

1. `arena::Bump::reset` (Miri-checked).
2. `workqueue` crate: bounded `Queue`, `Pool` of named threads with
   per-thread processor state, per-item `catch_unwind`, drop joins. The pool
   owns its queue, so a pool with no threads is paused (full), not closed.
3. `messages::transaction::Transaction`, depth-checked against the
   reference at the boundary (`--depth-vectors` in the oracle).
4. `EpochState` and `TransactionValidator` (one reused 64 KiB arena),
   on a single processor thread; one work item per request.
5. `SubmitTransaction`: request checks, decode in the handler, enqueue
   (`resource_exhausted` when full), await verdicts; valid transactions
   answer `unimplemented("consensus submission")`. Epoch state from the
   command line.
6. Tests:
   - `validator/tests/submit.rs`: every `tx_data` vector at the latest
     version over gRPC, with the reference's verdict (error kind in the
     status message); malformed requests; a full queue.
   - `validator/tests/processor_allocations.rs`: a warm processor accepts
     every accepted vector with zero heap allocations.
   - `tools/validator-client`: sui's client submits a signed transfer
     (`unimplemented`: consensus submission) and one with too low a budget
     (`GasBudgetTooLow`).

## Notes

- Performance: `BENCHMARKS_PHASE5.md`.
- Transactions are parsed without their digest (`DigestPending`); the
  processor hashes those that validate.

- Gas price under RGP is not a `validity_check` failure (the reference
  checks it separately), so the processor accepts it, as the PRD scopes it.
- Pings still `todo!()`: they need a consensus position.

## Remaining

(none)
