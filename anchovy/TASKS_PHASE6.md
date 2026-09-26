# Phase 6 progress: signature verification; processors share threads

Plan: `IMPLEMENTATION_PLAN_PHASE6.md`. Branch `mlogan-phase6`, to be merged
into `anchovy-main`.

## Status: plan written, step 1 next

## Done

(nothing yet)

## Remaining

1. `workqueue`: `queue`/`Inbox`, `Worker` with several processors; `Pool`
   removed.
2. `EpochState` with the verifier; `Rejected`.
3. Validation over `SenderSignedData`, forwarding; the verifier processor;
   one worker for both.
4. Tests: pipeline, gRPC, allocations, validator-client.
5. Benchmark and notes.
