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
   1. `store` split into the database (`db.rs`) and the cache (`cache.rs`).
      Done: model tests (with and without eviction) and race tests for
      the object and transaction tickets. `Message` now shares its buffers
      (an inner `Arc`, as the reference's `Object`), so the cache hands out
      clones. All callers use `write_through` for now.
   2. The executor writes through the cache (`Store::write`). Done; the
      legacy `ScheduleExecutor` and standalone validator keep `write_through`.
   3. `CheckpointCommitter` and the commit batch. Done:
      - checkpoint signatures from voted blocks too, duplicates dropped
        before parsing;
      - the aggregator certifies in sequence order and hands certificates
        to the committer (no commit waiter: simpler);
      - the checkpoint record and certified summary move into the commit
        batch; genesis sets the committed watermark;
      - built checkpoints and chunks pruned below the committed one.
      The restart test is ignored until step 4.
   4. Restart by replay. Next.
   5. Backpressure.

## TODOs

- Resubmit this validator's checkpoint signature until it is seen
  processed, as the reference's consensus adapter does.
- Bound the summary waiter: pre-commit signatures can name any sequence
  number.
- The executor parses its outputs, and `Store::write` parses them again.
