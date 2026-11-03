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
- The forwarding address registry's implicit version (#27989), lazily: each
  user transaction not naming the registry carries the generation it would
  read (`RegistryRead`, placed by `registry_reads` as `link` places
  accesses), or the registry's epoch-start version if nothing touched it
  this epoch. Execution waits for the generation only if it reads the
  registry (`LazyVersion`, `GenerationTable::wait`); causal admission's
  reserved slot runs the transaction recording it. A declared registry is
  read at its input's version. The synced path takes the version from
  effects and waits for it as an input. Sui's reference executor (testing)
  leaves the registry out: nothing in sui reads it implicitly yet.

## Remaining
- Full suite and compat sweeps under derp.
- Composite workload seed 6 (stalls) panicked once: a settlement's
  type-argument package was missing from the store (`MissingNative`). The
  replay passed, so the failure isn't deterministic. Investigate.
- #28520 (sui-core pulls consensus-pool transactions only while their owned
  gas versions are current) is not ported: it concerns proposal budget, not
  execution. Look at anchovy's admission for the same gap later.

## Elsewhere
- Processor logging (`Brief`, trace logs) is on branch
  `mlogan-processor-logging`, set aside.
