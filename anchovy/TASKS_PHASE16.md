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

## Forks found and fixed

(none yet)

## Open
