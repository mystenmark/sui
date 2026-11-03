# Phase 25: settlement in Rust

Sui main executes accumulator settlement transactions in Rust instead of the
Move VM (#28538, which replaced the batching of #28452). Anchovy follows.

## Done
- Merged sui main through #28553 (7 commits past anchovy-main's base).
- `executor::accumulator_settlement`: a port of the reference's Rust
  settlement onto anchovy's types. `ProgrammableSystemTransaction` runs it
  instead of the Move VM.
- Conservation checks skip layout resolution for settlement's SUI-free
  objects (`has_no_sui`).

## Remaining
- Full suite and compat sweeps under derp.
- #28520 (sui-core pulls consensus-pool transactions only while their owned
  gas versions are current) is not ported: it concerns proposal budget, not
  execution. Look at anchovy's admission for the same gap later.

## Elsewhere
- Processor logging (`Brief`, trace logs) is on branch
  `mlogan-processor-logging`, set aside.
