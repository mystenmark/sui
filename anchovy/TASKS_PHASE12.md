# Phase 12 progress: the writeback cache

Goal: transaction outputs are not written to the database until a certified
checkpoint containing them executes, so incorrect execution outputs are
never persisted, and commits become large write batches. The cache is the
sole point of access to the object database: every read goes through it,
seeing uncommitted outputs, and it caches reads positively and negatively.

## Status: implementing (design agreed)

## Steps

1. Spec of the reference writeback cache, point by point, with its bug
   history: `docs/writeback_cache_reference.md` (sub-agent). Done.
2. A design for a from-scratch re-implementation in anchovy, citing the
   spec: `docs/writeback_cache_design.md`. Agreed: restart by replaying
   the epoch from its start (to be replaced later by persisted handler
   state); the uncommitted set is empty at epoch end.
3. Implementation, in the design's order of work:
   1. `store` split into the database and the cache. In progress.
   2. The executor writes through the cache.
   3. `CheckpointCommitter`, the commit waiter, the commit batch.
   4. Restart by replay.
   5. Backpressure.
