# Phase 10: a generalized waiter

Requested after phase 9 (not in `PRD.md`): infrastructure for work that
waits on other things. Examples:
- a transaction waits for its input objects;
- checkpointing waits for effects.

A work item is enqueued with the keys it waits for. Other processors notify
keys as they become available. The waiter hands out an item once all its
keys are available.

The API is three batched methods: `wait_for`, `notify` and `get_ready`.

Decisions (asked):
- A key notified before anything waits on it is found by a check the caller
  supplies (for example, "is this object version in the store?"). The waiter
  keeps only pending waits.
- One thread owns the waiter: no locks; batches come through work queues.

## Semantics

- `wait_for(batch)`: each item, with its keys.
  - A key the waiter isn't tracking is checked with the caller's
    `Availability`. The batch's distinct untracked keys are checked in one
    call.
  - A key already tracked is pending: it was checked and found unavailable,
    and hasn't been notified since.
  - An item with no pending keys is ready at once.
  - A key repeated within an item counts once.
- `notify(keys)`: each key is available from now on.
  - Items waiting on it count it off, and an item with none left becomes
    ready.
  - A key nothing waits on costs one hash lookup and is forgotten.
- `get_ready(out)`: moves the ready items to `out`, in the order they became
  ready.

**Correctness:** producers make a key visible to the check before notifying
it. Then a wait is never lost:
- If the notification is handled before the wait, the check sees the key.
- If the wait is handled first, the key is pending and the notification wakes
  it.

Waits and notifications can arrive in any order, through one queue or
several.

## Data structures

Built for throughput: no locks, no allocation per item or per key in steady
state.

- **Keys:** a hash map (foldhash) from each pending key to its waiters.
  - A key with one or two waiters holds them inline; more spill to a vector.
  - Waiters are slot indices (`u32`).
- **Items:** a slab of slots, each the item and its count of pending keys,
  with a free list. A slot is reused once its item is ready.
- **Ready items:** a vector, drained by `get_ready`.
- **Batches:** a wait batch is flat: items, the end of each item's keys,
  then all keys. That's three vectors per batch, not one per item, and they
  are reused.

## The processor

`WaiterProcessor` runs the waiter on a worker thread, following the
existing processor architecture:
- Its input is a command: a wait batch, or a batch of keys to notify.
- After each command, the ready items go to its sink as one batch.

## Tests and benchmark

- **Unit tests** for each rule above.
- **A randomized model test** against a naive implementation: a set of
  available keys, and items rescanned on every notification.
- **Benchmark:** items waiting on one to four keys each, notified in
  shuffled batches. It reports nanoseconds and allocations per item and per
  key.
