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

- Execution inputs split (plan, "Execution inputs"): `ExecutionInputs`
  (checked against `InputObjects`), the temporary store over it, gas smashing
  and charging, early errors, the arena PTB builder (checked against
  sui-types'), `messages::Kept` and the store reads it lends views through.

## In progress

- Invariant checks and the type layout resolver (agent).
- Execution context, interpreter and `SPT::execute` (agent).
- The engine (`execution_engine.rs`, written; compiles once the above land).

## Remaining

- Loading inputs: for now in the executor processor, just before execution.
  A loader that runs ahead needs shared objects' versions assigned (consensus):
  loaded early, they would miss earlier transactions' writes.
- System transactions: epoch change, end-of-epoch kinds, authenticator state
  update, safe mode.
- TODO (deferred as too risky for now): reuse the input type resolution
  linkage across transactions, keyed on the transaction's package IDs. It is
  ~7% of a transfer after the epoch caches, but its result depends on the whole
  PTB's analysis, so a key that provably determines it needs care.

3. Static PTBs and the engine path for user PTBs; differential harness.
4. System transactions and genesis.
5. Mainnet replay; switch anchovy over; allocation tests; benchmark.
