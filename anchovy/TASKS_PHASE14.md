# Phase 14 progress: two stores

Goal: unify the stores the pipeline has grown into exactly two, as the
reference does (folding its checkpoint and other databases into the
perpetual one). Most current tables are unbounded, so they cannot live only
in memory.

- **Perpetual store:** objects, and everything else that outlives an epoch:
  checkpoints, transactions and effects, epoch records.
- **Epoch store:** the epoch's state, discarded at the end of the epoch.
  Strictly separated within it:
  - consensus output state, written only by the commit handler;
  - checkpoint builder state;
  - everything else.

## Status: design agreed; implementing step 1

## Steps

1. Survey: every store, table and stateful structure anchovy has today,
   and the reference's perpetual, epoch and checkpoint databases, with
   their write discipline (sub-agents).
2. Design: `docs/two_stores_design.md`. Agreed: discard-and-replay first,
   then quarantine; the commit handler links rounds; prune generations and
   state hashes in memory.
3. Implementation, in the design's order of work.
