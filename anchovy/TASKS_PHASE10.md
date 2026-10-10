# Phase 10 progress: a generalized waiter

See `IMPLEMENTATION_PLAN_PHASE10.md`.

## Status: done

## Done

1. `crates/waiter`: `Waiter` (`wait_for`, `notify`, `get_ready`),
   `WaitBatch`, and `Availability` (any `FnMut(&K) -> bool` is one).
2. Unit tests for each rule, and a randomized model test. Mutations that
   lose a wakeup or a wait fail it; skipping the dedup of a repeated key does
   not, because counting a key twice and counting it off twice comes to the
   same thing.
3. `WaiterProcessor`, tested on a worker thread.
4. Benchmark (`cargo bench -p waiter`): items of one to four keys (2.5 on
   average), keys the size of an object version shared by about two items
   each, notified in shuffled batches of 1000. Steady state, after a
   first round that sizes the tables:

   | pending items | wait_for ns/item | notify ns/key | allocs/item |
   |---|---|---|---|
   | 1,000,000 | 102 | 81 | 0.06 |
   | 100,000 | 84 | 42 | 0.07 |
   | 10,000 | 53 | 35 | 0.06 |

   From the first version (148 ns/item and 115 ns/key at 1M, 0.71
   allocs/item):
   - `wait_for` looks each key up once, not twice, and moves keys out of
     the batch instead of cloning them. A key not pending yet is registered
     provisionally, then released after the batch's one availability check
     if the key is available.
   - Waiter lists of three or more come from a pool.

   **Tried and dropped:** moving those lists out of the table entry, behind
   an index. It made entries smaller, but the extra indirection doubled
   notify time at 1M items.

## Remaining

- Use it: transactions waiting on their input objects once execution is
  driven by consensus commits; checkpointing waiting on effects.
