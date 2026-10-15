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

## Status: implementing (design agreed)

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
      Next: step 2.
   2. The final checkpoint, with a genesis that has a system state.
   3. Reconfiguration across epochs, and the crash tests.
