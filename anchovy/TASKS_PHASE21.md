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
    balance at the chunk's root version, less the net withdrawals of earlier
    transactions not yet settled. Anchovy passes an empty view, so it allows
    withdrawals sui rejects, and a later settlement underflows.
  - A transaction can also read the root at its version before the previous
    chunk's barrier has written it (an undeclared dependency).
  - Design, as sui's current one (#27840, read on its `main`):
    - **Unsettled withdrawals.** An in-memory table of net withdrawals by
      (account, root version), recorded when a transaction executes, by
      either path, and released when its checkpoint commits. It needs no
      persistence: whatever survives a crash was settled.
    - **Blocking.** Execution blocks on the root at its version until a
      barrier writes it.
    - **Causal admission.** Each chunk transaction gets index (height, i);
      settlements and barriers get none and bypass admission. Whoever creates
      a chunk (the gate, or a synced split) registers its length before its
      transactions can reach the executor, so the consecutive successor is
      known: (h, len-1) is followed by (h+1, 0). The epoch's change-epoch
      transaction is a one-transaction unit at the last height + 1.
      Admission takes a transaction when fewer than K are in flight, or when
      its index is the watermark's successor (at most one over the limit),
      on K+1 executor workers. The chunk lock already gives each chunk one
      source, which is sui's accumulator-version deduplication.
  - Steps:
    - [ ] the unsettled withdrawals table;
    - [ ] execution state per worker (today one arena and runtime behind a
      lock);
    - [ ] causal admission and the executor pool;
    - [ ] blocking on the root's version.
- [ ] Then: submitting through `LocalValidatorAggregatorProxy` with
  amplification, so anchovy's RPC takes a share of submissions directly;
  epoch changes under load.

## Known gaps found

- Anchovy doesn't implement address aliases in voting or submission. It
  claims and expects no aliases, so it votes against a transaction whose
  sender has aliases, and rejects one signed by an alias. Its stake can't
  block either. Execution and checkpoints follow the quorum's claims.
- A rebase onto sui `main` (at protocol 141) breaks the build: anchovy's
  ported natives use gas algebra and fastcrypto APIs that changed. Protocols
  139-141 change execution (forwarding addresses, vector natives,
  references in PTBs, shuffled commit blocks, batched settlements). Catching
  up is the next phase; compat networks start at the highest version.
- The reference oracle in `Execution::execute_at` (testing builds) doesn't
  rewrite coin reservations, so it would disagree with anchovy's executor on
  such transactions.
