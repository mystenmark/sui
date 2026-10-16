# Phase 13 progress: reconfiguration

Goal: clear lifetimes for every component, and a simple, readable startup
and reconfiguration process.

- **Perpetual:** the gRPC server, the object store with its writeback cache,
  and state sync.
- **Per epoch:** everything else, including every processor and consensus.

The node's main loop:
1. Construct the perpetual components.
2. Construct the epoch's `EpochState` and `EpochParams`.
3. Construct the epoch's workers and pipeline, passing them
   `Arc<EpochState>`, `Arc<EpochParams>` and the perpetual services they
   need.
4. The gRPC server and state sync enqueue work into the current pipeline.
   During reconfiguration they `await` the next one, so the system pauses
   seamlessly.
5. Wait for "final checkpoint executed", drop the epoch's workers and
   pipeline, and return to step 2.

Out of scope for now:
- Deciding the end of the epoch in the commit handler. `EndOfPublish` is
  deprecated; the end is timestamp-based. Assume the handler at some point
  makes a final checkpoint.
- Backpressure (phase 12's last step, on hold).

## Status: implemented

## Steps

1. Spec of the reference's end of epoch and reconfiguration, with its bug
   history: `docs/reconfiguration_reference.md` (sub-agent). Done.
2. Design: `docs/reconfiguration_design.md`. Agreed.
3. Implementation, in the design's order of work:
   1. Lifetimes without an epoch end. Done: `Perpetual` and `Epoch`
      (dropping it clears `CurrentEpoch` and joins every worker), the
      epoch start record written at genesis, the gRPC server over
      `CurrentEpoch` (requests carry over to the next epoch), state sync's
      queue owned outside the pipeline. The node loop itself moves to step
      3, where an epoch can end.
   2. The final checkpoint. Done:
      - genesis from sui's genesis builder in the node tests; epoch
        records read from the system state; `EpochState` takes the
        epoch's start timestamp;
      - the timestamp stub, the reserved write of the system state,
        `EpochCloser`, `EndOfEpochData`, the committer's signal;
      - the state hasher seeded per epoch from the record (it had waited
        for a checkpoint 0 that never came).
      The test checks the root against the live object set independently.
   3. Reconfiguration across epochs. Done: `reconfigure`, `Node`
      (`start_epoch`, `finish_epoch`, `run`), consensus started per epoch
      through `ConsensusOutput`. Tests: epochs follow one another (roots
      checked against the live set at each end), crashes between the last
      checkpoint and the next record and inside the next epoch, and `run`
      left to itself for three epochs.

## Status after step 3

All steps of the design done. Open:
- the reference's other end-of-epoch transaction kinds, and protocol
  upgrades (capability votes);
- fork detection: done (in the built waiter, and every epoch drained
  before it is dropped);
- correct end-of-epoch determination (deferred transactions, randomness,
  the close deadline);
- a production genesis loader (the node tests build genesis with sui's
  builder);
- the epoch change and end-of-epoch transactions in anchovy's executor
  (the system transactions' port): until then sui's executor runs them.

## Architecture simplifications (after the review)

Done:
- The validator crate's pre-pipeline paths deleted: its standalone binary,
  the serial consensus path, `EpochHandle`'s optional consensus.
- The sequence, summary and fork waiters, all keyed on a checkpoint being
  built, merged into one built waiter routing by item kind.
- `Pipeline::start`'s wiring as a one-screen table (`node/src/wiring.rs`):
  queues declared once by name and item, shared dependencies in `Parts`.
- The pipeline executes with anchovy's executor. Sui's (`Reference`) runs
  genesis and the kinds the native one does not execute, and, with the
  `testing` feature the pipeline crates' tests enable, every transaction as
  an oracle that must agree.

Skipped: splitting the validator crate by role (it would move code more
than simplify it).
