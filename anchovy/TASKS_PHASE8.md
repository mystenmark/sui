# Phase 8 progress: an executor over zero-copy types

Plan: `IMPLEMENTATION_PLAN_PHASE8.md`. Branch `mlogan-phase8`, to be merged
into `anchovy-main`.

## Status: step 1 in progress

## Done

- Arena `IndexMap`, `IndexSet`, `BTreeSet` (`containers`), checked against
  `indexmap` and std over random operation sequences.
- `executor` crate: `ExecutionError` over the `ExecutionErrorKind` view; the
  gas model (15+), checked against sui-types over random charge sequences.

## Remaining

1. The executor's `Object`, missing `messages` views and writers (objects),
   execution modes.
2. Natives: verbatim copies, then the object runtime and object natives.
3. Static PTBs and the engine path for user PTBs; differential harness.
4. System transactions and genesis.
5. Mainnet replay; switch anchovy over; allocation tests; benchmark.
