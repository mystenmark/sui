# Phase 27: the full composite workload

Sui's `test_composite_workload` now mixes more than anchovy's composite test
did (phase 21's mix): party objects, shared deletion, receiving, upgrades,
owned objects and coin merges, gasless, sponsored and multisig
transactions, the deny list, allowed proposers and allowances.

## Done
- Every compat test but the composite ones under derp, seeds 1-6: all pass.
- `the_full_composite_workload_runs_alike`: the composite test mixed as
  sui's simtest mixes it now, SUI allowed in gasless transactions for every
  node. The phase 21 mix stays as the other two composite tests.
- Failure 1 (all seeds): gasless transactions panicked in execution with
  `GasPriceUnderRGP`. Sui-core's `check_gas` meters them at the reference
  gas price with the gasless computation cap as budget; both executors now
  do.

## Remaining
- The full composite test under derp, until seeds pass.
- Then longer sweeps: the full mix, and every compat test past seed 6.
- #28520 (consensus-pool owned-version validation) for anchovy admission.
