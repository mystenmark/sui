# Phase 8 progress: an executor over zero-copy types

Plan: `IMPLEMENTATION_PLAN_PHASE8.md`. Branch `mlogan-phase8`, to be merged
into `anchovy-main`.

## Status: steps 1-2 done (natives untested until the adapter runs them); step 3 next

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

## Remaining

3. Static PTBs and the engine path for user PTBs; differential harness.
4. System transactions and genesis.
5. Mainnet replay; switch anchovy over; allocation tests; benchmark.
