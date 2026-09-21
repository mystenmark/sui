# Anchovy Phase 1 progress

Plan: `IMPLEMENTATION_PLAN_PHASE1.md`. Branch `mlogan-anchovy`, worktree
`~/projects/mlogan-anchovy`. All work is under `anchovy/`, its own cargo
workspace; run cargo from there.

## Done

1. Workspace, lints, `Reader` with `bcs` 0.1.6 encoding rules, errors.
2. `Arena`, `Alloc` with `Measure` and `Build`, `WireBuf`, `Message`, `Wire`.
   Miri clean (`cargo +nightly miri test`).
3. Base types, `TypeTag`/`StructTag` (`TypeInput`/`StructInput` are aliases:
   with identifier checks deferred they are the same wire type).
4. Transaction types, system transaction kinds, `SenderSignedData`.
7. Objects. 8. Effects V1/V2, status and error enums, events.
9. Checkpoints, `CheckpointData`.
- `tests/mainnet.rs`: 64 mainnet checkpoints (2,332 transactions, 22,397
  objects) parse; each transaction, effects, events and object re-parses
  alone from its recorded span to an equal view. One checkpoint is checked
  in; `scripts/fetch-mainnet.sh` fetches the rest into `corpus/` (ignored).

## Remaining

5. Transaction index built at parse time; then the effects index.
6. `MultiSig`, `MultiSigPublicKey`, `CompressedSignature`, `PublicKey`
   views (not called during message parsing).
10. Builders, snapshot equality test, view-to-builder conversion.
11. Differential tests, in-tree mutation fuzzer, `cargo fuzz` targets with
    an allocation-counting allocator.
12. Benchmarks against the serde baseline; optimize.
13. `tools/sui-oracle` vectors for derived data.
- Unit tests that each `MIN_WIRE_SIZE` is a true lower bound (build the
  minimal value, check its length). An overestimate rejects valid input.

## Notes

- The snapshot on `origin/main` (4e6ff818cf) has additions over the commit
  the strictness notes were written against: `AllowedProposers`,
  `TransactionExpiration::Validity`, `WithdrawFrom::SenderAllowance`,
  `ForwardingAddressRegistryCreate`, `CommandArgumentError::InvalidTxContext`.
- Container depth is counted only on paths that can reach a `TypeTag`,
  since nothing else recurses.
- `Ref<'a, T>` is an arena box that is empty in the measure pass; parsers
  must not dereference what they build.
- CheckpointData arena is about 43% of wire size on mainnet, mostly
  `Object` views.
- The nightly toolchain and its miri component were installed for Miri.
