# Anchovy Phase 1 progress

Plan: `IMPLEMENTATION_PLAN_PHASE1.md`. Branch `mlogan-anchovy`, worktree
`~/projects/mlogan-anchovy`. All work is under `anchovy/`, its own cargo
workspace; run cargo from there.

## Done

1. Workspace, lints, `Reader` with `bcs` 0.1.6 encoding rules, errors.
2. `Arena`, `Alloc` with `Measure` and `Build`, `WireBuf`, `Message`, `Wire`.
   Miri clean (`cargo +nightly miri test --lib`).
3. Base types, `TypeTag`/`StructTag` (`TypeInput`/`StructInput` are aliases:
   with identifier checks deferred they are the same wire type).
4. Transaction types, system transaction kinds, `SenderSignedData`.
5. `TransactionIndex`, built while parsing: shared inputs, owned inputs with
   gas, packages (sorted, deduplicated), receiving, move calls, funds
   withdrawals, coin reservations. `tests/tx_index.rs` checks it against a
   walk of the parsed transaction on 2,109 mainnet transactions.
   Effects: each V2 change carries a `ChangeKind` worked out at parse time;
   `TransactionEffectsV2::changes(kind)` replaces `created()`, `mutated()`
   and the rest.
6. `signature.rs`: `MultiSig`, `MultiSigPublicKey`, `CompressedSignature`,
   `PublicKey` (with the `Passkey` variants the snapshot omits). Not called
   during message parsing.
7. Objects. 8. Effects V1/V2, status and error enums, events.
9. Checkpoints, `CheckpointData`.
11. (part) `tests/mutate.rs`, a deterministic mutation fuzzer run as a test
    (`ANCHOVY_MUTATE_ITERATIONS` to run longer; 3M per test is clean), and
    `fuzz/`, a cargo-fuzz target whose allocator asserts at most one
    allocation per parse and at most 32 arena bytes per input byte. 20M
    runs under ASan, clean. Seed it with
    `cargo run --example seed_fuzz_corpus`, run with
    `cd fuzz && cargo +nightly fuzz run parse`.
- `tests/mainnet.rs`: 64 mainnet checkpoints (2,332 transactions, 22,397
  objects) parse; each transaction, effects, events and object re-parses
  alone from its recorded span to an equal view. One checkpoint is checked
  in; `scripts/fetch-mainnet.sh` fetches the rest into `corpus/` (ignored).

## In progress

10. Builders (`src/build/`), snapshot equality test, view-to-builder
    conversion, byte-exact round trips over the corpus.
12. `benches/parse.rs` is written; its baseline needs the builders.

## Remaining

11. Differential fuzz target against `bcs::from_bytes` on the builders.
12. Record benchmark numbers; optimize.
13. `tools/sui-oracle` vectors for derived data. Until then the index and
    the change classes are checked only against this crate's own reading of
    the reference's rules.
- Unit tests that each `MIN_WIRE_SIZE` is a true lower bound. An
  overestimate rejects valid input.

## Notes

- The snapshot on `origin/main` (4e6ff818cf) has additions over the commit
  the strictness notes were written against: `AllowedProposers`,
  `TransactionExpiration::Validity`, `WithdrawFrom::SenderAllowance`,
  `ForwardingAddressRegistryCreate`, `CommandArgumentError::InvalidTxContext`.
  The accessor rules in the notes may be stale in the same places.
- Container depth is counted only on paths that can reach a `TypeTag`,
  since nothing else recurses.
- `Ref<'a, T>` is an arena box that is empty in the measure pass; parsers
  must not dereference what they build. Code that has to read parsed values
  back (the transaction index fill) is gated on `Alloc::BUILD` and must make
  the same reservations in both passes.
- Index counts come only from values that exist in both passes: enum
  variants, wire-backed slices, and `Reader::struct_tags()`.
- The index differs from the reference where the reference errors: a
  duplicated input object is kept twice and left for validation to reject.
  It also reports owned, shared and package inputs as three slices, so the
  reference's interleaved `input_objects()` order is not recoverable.
- Effects on mainnet include a change shape the reference lists under no
  accessor: `(NotExist, NotExist, Created)`, created and destroyed in one
  transaction. It is `ChangeKind::Transient`.
- Worst-case arena growth is 80 bytes (`Command`) for a 3-byte
  `MakeMoveVec(None, [])`, about 27x. Boxing `MoveCall` would halve it at
  the cost of an indirection on the most common command.
- CheckpointData arena is about 48% of wire size on mainnet, mostly `Object`
  views; the transaction index is about 600 bytes per transaction.
- `MultiSig` parses only through `Message`, which owns its buffer. Validation
  will want to parse signature bytes in place with a scratch arena.
- The nightly toolchain, its miri component and `cargo-fuzz` were installed.
