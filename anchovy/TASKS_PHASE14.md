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

## Status: implemented

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
4. The generation table, built checkpoints and state hashes in the epoch
   store, unpruned; settled chunks in memory, as they are bounded; the Move
   runtime replaced past 1024 packages. Done. The rule: bounded state in
   memory, unbounded epoch state in the epoch store, never epoch state in
   the perpetual store.
5. Quarantine and resume (`docs/quarantine_design.md`), following the
   reference. Done: the quarantine and commit record; generations written
   with their checkpoint; the object store's commit, then the epoch
   store's; resume from the record with a prior-commit window; built
   checkpoints usable once built in this run or committed; state hashes
   above the committed checkpoint deleted at startup, missing roots folded
   again; the root's round links in the commit handler. Tests of the
   quarantine, and of crashes mid-epoch and with commits waiting.

Notes:
- The test suite takes about 60s, from about 40s: each test opens epoch
  stores, and Tidehunter databases opened in parallel contend. Isolated,
  tests are as fast as before.
- nextest occasionally reports a test as leaky under load, apparently
  Tidehunter's background threads outliving the test by its 100ms leak
  timeout.

## Batched reads (decided: no)

`cargo bench -p store --bench reads`: object reads (as the input waiter's)
and processed-digest lookups (as the commit handler's), one at a time,
sorted, or spread over threads, in batches of 1 to 1024, warm and cold.
Tidehunter has no multi-get (nor upstream main; sui's typed-store loops
`get`), so a batch is only faster with its reads in flight together:
sorting gains nothing; threads gain up to 6x on cold object reads from 64
keys up, nothing on epoch-store lookups, whose index is in memory.

Decision: reads stay one at a time. If the input waiter needs more read
throughput, several threads can serve its queue.

