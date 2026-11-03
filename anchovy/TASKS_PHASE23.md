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
- [ ] Compat at 139.
- [ ] Protocol 140: vector natives (done), shuffled commit blocks (glue),
  references in PTBs, ID leak verifier off, object funds checked in
  execution, no effects dependencies: all already follow the flags.
  Staggered submission (#27875, #27882) only paces a validator's own
  submissions, and its signal flag is off everywhere: not ported.
- [ ] Protocol 141: natives done; the Clock read implicitly (#28241):
  - the commit handler assigns each transaction the Clock version the
    commit's prologue writes (one more per prologue), kept in the commit
    record across restarts;
  - entries carry it to execution, which loads the Clock at it, waiting
    (as for the root) until written;
  - the synced path takes it from effects; the oracle passes it on.
- [ ] Batched settlements (#28452), the invariants refactor (#28388).

## Known gaps

- Compat tests that publish packages fail at 138 alike on both sides
  (`VMVerificationOrDeserializationError`): the new compiler's bytecode at
  an old version; recheck at 140, where the ID leak verifier is off.
- The composite workload forked at 138 (checkpoint 118); recheck at 141.
