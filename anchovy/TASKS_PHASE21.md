# Phase 21: the composite workload

The reference's composite workload (`sui-benchmark`, `workloads/composite`)
run against the compat network, two sui validators and anchovy. It mixes
the operations sui's `test_composite_workload` simtest mixes:
- shared counter increments and reads, randomness reads;
- address and object balance deposits, withdrawals and overdraws;
- test coin mints, deposits and withdrawals;
- accumulator balance reads, authenticated events;
- coin reservation withdrawals, address aliases;
- conflicting transactions and address-balance gas.

It comes before mainnet replay: it covers more of what anchovy runs, and
its forks show where anchovy and the reference disagree.

`sui-benchmark` is a dev-dependency of compat only; production code still
uses only the approved crates.

## Plan

- [x] `the_composite_workload_runs_alike`:
  - a sui full node with RPC;
  - the workload for 60 s at 20 qps, submitted through the full node;
  - then anchovy certifies the quorum's last checkpoint;
  - it asserts the workload succeeded, failed permanently, overdrew and was
    cancelled.

  Ignored until it passes. Fork reports now list contents entries that
  differ by position.
- [x] Fork 1, checkpoint contents: anchovy listed every signature with no
  alias version. Sui lists the claimed signatures with the versions in each
  transaction's `AddressAliasesV2` claim. The claim now travels from the
  commit decoder, through deferral and settlement, to the contents builder.
- [x] Panic 2, coin reservations: execution didn't rewrite coin reservation
  inputs into withdrawals, so linkage analysis looked up the masked id as an
  object. The rewrite now happens as the reference's: the field is read at
  the commit's accumulator version, and the owner must be the sender.
- [ ] Fork 3, object funds:
  - At 138 off mainnet (`check_object_funds_withdraw_in_execution`), an
    object balance withdrawal is checked in execution against the settled
    balance less the net withdrawals of earlier transactions not yet
    settled (`UnsettledObjectWithdrawals`).
  - Anchovy passes an empty view, so it allows withdrawals sui rejects; a
    later settlement underflows.
  - Plan: a shared table of unsettled net withdrawals by (account,
    accumulator version), recorded after each successful execution from
    its effects (and from stored effects when a transaction is found
    executed already, as generations are), read by the executor, and
    pruned when a checkpoint commits. Execution order within a commit is
    already deterministic: two withdrawals from one account conflict on its
    owning object.
- [ ] Then: submitting through `LocalValidatorAggregatorProxy` with
  amplification, so anchovy's RPC takes a share of submissions directly;
  epoch changes under load.

## Known gaps found

- Anchovy doesn't implement address aliases in voting or submission. It
  claims and expects no aliases, so it votes against a transaction whose
  sender has aliases, and rejects one signed by an alias. Its stake can't
  block either. Execution and checkpoints follow the quorum's claims.
