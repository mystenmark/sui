# Phase 16 progress: compatibility with the reference implementation

Goal: anchovy and sui validators in one network, agreeing on every
checkpoint. Start with one of each (equal stake: a quorum needs both, so a
checkpoint certifies only if both built it the same), fix each fork in turn.
Most fixes are expected in the consensus handler; keep it clean and
straightforward as features accumulate.

Branched from `mlogan-phase15` (consensus integration, not yet merged).

## Harness

`crates/compat/tests/one_of_each.rs`: sui's `ConfigBuilder` makes a genesis
of two validators and their node configs; sui runs the first in-process
(`sui_swarm::memory::Node`), anchovy the second, with that validator's keys
and the same genesis. When checkpoints stop certifying, the test reports
each side's progress and the first checkpoint they built differently: both
summaries, the transactions only one lists, and the effects of those both
list differently.

Tests: an idle network certifies checkpoints; transfers submitted through
anchovy execute alike on both and certify.

## Forks found and fixed

1. Checkpoint 1 cut differently: the reference starts each epoch with no
   checkpoint cut (`last_checkpoint_flush_timestamp` 0), so an epoch's first
   commit always cuts one; anchovy counted the interval from the previous
   checkpoint.
2. The prologue's `additional_state_digest`, at the first user transaction:
   the reference folds in each scheduled user transaction's execution time
   estimate (`IndirectStateObserver`). Added `consensus::execution_time`
   (the reference's `ExecutionTimeEstimator`); the commit stage takes each
   commit's `ExecutionTimeObservation`s (checked against their block's
   author) before estimating its transactions; observations are written with
   the consensus output and restored at start-up; voting rejects durations
   the reference cannot deserialize.

## Open

Known divergences, not yet reached by a test:
- Chunk heights restart at 0 each epoch in the reference, and the
  settlement barrier carries the height; anchovy continues them.
- Epoch change: observations stored at the previous epoch's end are not
  loaded at the epoch's start, nor stored by the change-epoch transaction;
  capabilities (protocol upgrades) are ignored.
- Deferral and cancellation: congestion control (budgets from the
  estimates), owned-object double-spend deferral, unpaid amplification
  deferral, randomness after DKG failure (likely in a mixed network: anchovy
  takes no part in DKG).

Next tests: shared objects, transactions submitted to sui, a crash and
restart of each side, an epoch change.
