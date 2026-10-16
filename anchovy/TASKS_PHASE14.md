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

## Status: discard-and-replay done (steps 1-4); quarantine next

## Steps

1. Survey: every store, table and stateful structure anchovy has today,
   and the reference's perpetual, epoch and checkpoint databases, with
   their write discipline (sub-agents).
2. Design: `docs/two_stores_design.md`. Agreed: discard-and-replay first,
   then quarantine; the commit handler links rounds; prune generations and
   state hashes in memory.
3. Implementation, in the design's order of work.

## Progress

1. The epoch store: per-epoch directory of two Tidehunter databases,
   `replayed` (emptied on restart) and `kept` (start versions); old epochs
   deleted after reconfiguration. Done.
2. Perpetual epoch records by epoch; start versions in the epoch store.
   Done.
3. Consensus output: processed digests, owned locks and stream links through
   the commit handler's writer, one batch per commit; the resolver keeps no
   state. Done.
4. The generation table, settled chunks, built checkpoints and state hashes
   in the epoch store, unpruned (the epoch store is dropped with the epoch);
   the Move runtime replaced past 1024 packages. Done. (A first version
   pruned in memory; replaced, as nothing in the epoch store needs pruning.)
5. Quarantine and resume, with the `commit` record. Next, after its design.

Notes:
- The test suite takes about 60s, from about 40s: each test opens epoch
  stores, and Tidehunter databases opened in parallel contend. Isolated,
  tests are as fast as before.
- nextest occasionally reports a test as leaky under load, apparently
  Tidehunter's background threads outliving the test by its 100ms leak
  timeout.

