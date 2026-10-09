# Phase 8 progress: an executor over zero-copy types

Plan: `IMPLEMENTATION_PLAN_PHASE8.md`. Branch `mlogan-phase8`, to be merged
into `anchovy-main`.

## Status: steps 1-2 done; step 3 in progress (translation done, execution next)

## Done

- Arena `IndexMap`, `IndexSet`, `BTreeSet` (`containers`), checked against
  `indexmap` and std over random operation sequences.
- `executor` crate: the gas model (15+), checked against sui-types over random
  charge sequences.
- `messages`: object writers and BCS sizes (every corpus object rebuilds byte
  for byte), `MoveObject::id`, `Display` for ids, `Ord`/`Hash` on type tags.
- `exec-types`: what natives and executor share (errors, system ids, id
  derivation, `Object`, storage traits, metadata, `TxContext`, type tag
  conversions), checked against sui-types where it computes anything.
- `natives`: copied verbatim, then the object runtime and object natives
  ported (byte fingerprints; test scenario natives left out).

- Adapter pieces independent of execution inputs: package stores, linkage,
  `Env`, gas meter, error conversion, execution results, accumulator helpers,
  coin deny list (tested against sui-types), effects construction (tested
  against sui-types), the store-independent half of `GasCharger`.
- PTB translation: metering, loading, typing, verify passes and invariant
  checks (`translate_and_verify`). Untested until the differential harness.
- Execution values and locals; `adapter.rs` (Move runtime, native
  extensions, metered verifier).

## Remaining

- Execution context and interpreter: the context loads inputs from the state
  view and writes results back, so it waits on the inputs interface below.
- Deferred until the input-loading processor's interface is designed: the
  temporary store, gas smashing and final charging, the engine paths.

3. Static PTBs and the engine path for user PTBs; differential harness.
4. System transactions and genesis.
5. Mainnet replay; switch anchovy over; allocation tests; benchmark.
