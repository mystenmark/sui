# Phase 8 progress: an executor over zero-copy types

Plan: `IMPLEMENTATION_PLAN_PHASE8.md`. Branch `mlogan-phase8`, to be merged
into `anchovy-main`.

## Status: plan written, step 1 next

## Done

(nothing yet)

## Remaining

1. Containers (arena `IndexMap`/`IndexSet`), missing `messages` views and
   builders; the executor crate skeleton with the gas model, errors,
   execution status and modes.
2. Natives: verbatim copies, then the object runtime and object natives.
3. Static PTBs and the engine path for user PTBs; differential harness.
4. System transactions and genesis.
5. Mainnet replay; switch anchovy over; allocation tests; benchmark.
