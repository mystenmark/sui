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

5. Fan-in (keys per item) by fan-out (items per key) at a fixed number of
   item-key edges, every key used by exactly fan-out items, nothing visible.
   Steady state, no allocations in any shape. Nanoseconds per edge:

   2M edges (the key table out of cache when keys are many):

   | fan-in \\ fan-out | 1 | 8 | 64 | 512 |
   |---|---|---|---|---|
   | 1 | wait 30.8, notify 48.8 | 32.3, 13.5 | 14.2, 4.8 | 11.6, 3.2 |
   | 8 | 27.4, 38.1 | 19.4, 6.5 | 10.0, 2.3 | 7.4, 1.7 |
   | 64 | 24.9, 33.4 | 18.6, 4.7 | 9.8, 1.4 | 7.2, 0.9 |
   | 512 | 24.4, 32.6 | 20.8, 4.4 | 11.0, 1.4 | 8.1, 0.6 |

   100K edges (in cache):

   | fan-in \\ fan-out | 1 | 8 | 64 | 512 |
   |---|---|---|---|---|
   | 1 | wait 12.5, notify 13.5 | 15.0, 3.8 | 9.0, 2.2 | 7.2, 1.8 |
   | 8 | 10.9, 13.7 | 12.8, 3.7 | 7.1, 1.5 | 5.9, 1.0 |
   | 64 | 10.5, 11.9 | 12.6, 2.4 | 7.1, 0.9 | 7.7, 0.4 |
   | 512 | 11.9, 12.0 | 12.3, 2.5 | 8.6, 0.8 | 9.7, 0.2 |

   Cost per edge is set mostly by the number of distinct keys, which decides
   whether the key table fits in cache.
   - **High fan-out:** few keys, so lookups hit. Notification amortizes one
     table removal over many waiters, down to well under a nanosecond per
     edge.
   - **Fan-out 1 with many keys:** the worst case, because every edge is a
     table miss.
   - **Fan-in:** barely matters per edge; an item's cost grows linearly with
     its keys.

## Remaining

- Use it: transactions waiting on their input objects once execution is
  driven by consensus commits; checkpointing waiting on effects.
