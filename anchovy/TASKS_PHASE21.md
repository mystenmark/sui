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
- [x] Fork 3, object funds:
  - At 138 off mainnet, sui checks object balance withdrawals in execution:
    against the settled balance at the chunk's root version, less the net
    withdrawals of earlier transactions not yet settled.
  - Built as sui's current design (#27840, read on its `main`):
    - [x] the unsettled withdrawals table, recorded on execution and
      released at checkpoint commit;
    - [x] native execution state per concurrent execution;
    - [x] causal admission and the executor pool: (height, position) indices
      with registered chunk lengths; settlements and barriers on a worker of
      their own;
    - [x] execution blocks on the root's version (`RootVersions`).
- [x] Fork 4, root reads: a declared read of the accumulator root in a
  commit's later chunk read the first chunk's generation. Sui assigns the
  root's next version, which each chunk's settlement advances.
- [x] Fork 5, funds versions: anchovy reserved and settled funds per commit.
  Sui does it per chunk (its accumulator version is the root's), so a split
  commit's later chunk saw an earlier chunk's reserved maximum instead of
  what it withdrew. Versions are now root versions, settled per chunk;
  `last_of_commit` went.
- [x] The reference oracle (testing builds) rewrites coin reservations too.
- [x] The composite test passes: 9 of 9 runs since the funds fix, 2 of them
  with the oracle.
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
- Sui's own writeback cache sometimes fails its consistency check on the
  accumulator root under the composite workload, at our base: a node's
  execution task panics. Anchovy is unaffected and the test still passes.
