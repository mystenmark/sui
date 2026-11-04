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

- With the fix, the full mix passes seeds 1-6. Its op sets show successes
  for every part of the mix; it now asserts them, but for package upgrades
  and the deny list, which the workload tries only a few times a run.
- Every permanent rejection in the full mix came from a sui validator, as
  the workload intends (overdraws, sponsor signatures, expiry, stale
  objects, allowed proposers).
- Compat never served anchovy's validator API at its network address: sui's
  clients got transport errors from it and submitted only to sui's
  validators, so anchovy saw the workload through consensus alone. It now
  serves it, as anchovy-node does.
- With anchovy taking submissions, the full, plain and stalled composite
  tests pass seeds 1-12 each.

## Remaining
- Anchovy's submit rejections carry no reason (`RawRejectedStatus { error:
  None }`), which sui's client decodes as a deserialization error: its
  validation errors don't map to the reference's `SuiError` yet. Clients
  can't tell a retriable rejection from an invalid transaction. Not a fork.
- Then longer sweeps: the full mix, and every compat test past seed 6.
- #28520 (consensus-pool owned-version validation) for anchovy admission.
