# Phase 23: catching up with sui `main`

Sui `main` (9d1756fede, protocol 141) merged into anchovy (`546a17ed55`):
anchovy changes only `anchovy/`, so the merge is clean. A merge, not a
rebase, so that the PR needs no force push of `anchovy-main`.

The ported crates follow their sources' upstream changes since the old base
(ab5057624b), replayed with 3-way merges:
- `natives` from `sui-execution/latest/sui-move-natives`;
- `executor` from `sui-execution/latest/sui-adapter`;
- `exec-types` from the parts of `sui-types` it ports.

## Plan

- [x] Build: replay the upstream changes; anchovy still supports up to 138.
  - natives: gas algebra (#28274, #28398), native vector operations
    (#27829, #28271), fastcrypto (#28502), the Clock borrowed without an
    input (#28241), `object::is_shared_input` (#28417),
    `derived_object::release` (#28418), groth16 (#28230), secp256r1
    ecrecover (#28460), the implicit system object resolver (#28244).
  - exec-types, executor: package read cost (#28224), linkage memoization
    (#28363), settlement type-node limit removed (#28307).
  - Test genesis at anchovy's cap; validity, mutation and input vectors
    regenerated (the oracle follows sui's fastcrypto).
- [x] Retroactive 138 changes: allowances on mainnet (#28049, config only),
  merged colliding deferrals (#28131), mainnet storage rebate (config).
- [x] Protocol 139:
  - the forwarding address registry as a shared input (#27989); its
    implicit version is not assigned, as nothing reads it implicitly yet;
  - DKG completing after its timeout (#27328);
  - ML-DSA-65 signatures and multisig members (#27810, #28270), with
    `fastcrypto-pq`;
  - storage rebate, package read cost: config.
- [x] Compat at 139: only the tests publishing `object_basics` fail, alike
  on both sides: the package now calls `clock::borrow`, which the framework
  has only from 141.
- [x] Protocol 140 (compat as at 139): vector natives (done), shuffled commit blocks (glue),
  references in PTBs, ID leak verifier off, object funds checked in
  execution, no effects dependencies: all already follow the flags.
  Staggered submission (#27875, #27882) only paces a validator's own
  submissions, and its signal flag is off everywhere: not ported.
- [x] Protocol 141: natives done; the Clock read implicitly (#28241):
  - the commit handler assigns each user transaction the Clock version the
    commit's prologue writes (one more per prologue), kept in the commit
    record across restarts;
  - the resolver makes the transaction wait on the Clock at that version
    (a waiter key, not a block in execution), and execution reads it there;
  - the synced path takes the version from effects; the oracle passes it on.
- [ ] Deferred: batched settlements (#28452, observably identical, a
  speedup) and the expensive-checks refactor (#28388, debug checks).
- [x] Full suite at 141: all 478 pass, the composite workload among them.

## Known gaps

- The composite workload forked once at 138 (checkpoint 118), in the first
  run after the merge; it passed at 139 and 140.
- The forwarding address registry's implicit version is not assigned.
- Staggered submission is not ported.
