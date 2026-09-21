# Anchovy Phase 1 progress

Plan: `IMPLEMENTATION_PLAN_PHASE1.md`. Branch `mlogan-anchovy`, worktree
`~/projects/mlogan-anchovy`. All work is under `anchovy/`, its own cargo
workspace; run cargo from there.

## Status: plan complete, pending review

Every step of the implementation order is done. What is not done is listed
under "Open" below.

## Done

1. Workspace, lints, `Reader` with `bcs` 0.1.6 encoding rules, errors.
2. `Arena`, `Alloc` with `Measure` and `Build`, `WireBuf`, `Message`, `Wire`.
   Miri clean on the unit tests and on a full checkpoint parse
   (`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --test tx_index`).
3. Base types, `TypeTag`/`StructTag` (`TypeInput`/`StructInput` are aliases:
   with identifier checks deferred they are the same wire type).
4. Transaction types, system transaction kinds, `SenderSignedData`.
5. `TransactionIndex`, built while parsing: shared inputs, owned inputs with
   gas, packages (sorted, deduplicated), receiving, move calls, funds
   withdrawals, coin reservations. Effects: each V2 change carries a
   `ChangeKind`; `TransactionEffectsV2::changes(kind)` stands in for
   `created()`, `mutated()` and the rest.
6. `signature.rs`: `MultiSig`, `MultiSigPublicKey`, `CompressedSignature`,
   `PublicKey` (with the `Passkey` variants the snapshot omits). Not called
   during message parsing.
7. Objects. 8. Effects V1/V2, status and error enums, events.
9. Checkpoints, `CheckpointData`.
10. `src/build/`: owned serde mirrors of all 124 snapshot types.
    `tests/format.rs` traces them with serde-reflection and asserts
    equality with the snapshot. `From<&view>` for every view.
    `tests/roundtrip.rs`: every corpus checkpoint, and each transaction,
    effects, events and object standalone, decodes with `bcs` to the value
    the view converts to, and re-encodes to the same bytes.
    `tests/min_wire_size.rs`: every `MIN_WIRE_SIZE` equals the encoded size
    of the smallest value.
11. Fuzzing. `tests/mutate.rs` is a deterministic mutation fuzzer run as a
    test (`ANCHOVY_MUTATE_ITERATIONS` to run longer; 3M per test clean).
    `fuzz/` has two cargo-fuzz targets: `parse` asserts at most one
    allocation per parse and at most 32 arena bytes per input byte (20M runs
    under ASan, clean); `differential` holds the parsers to `bcs` on the
    builders for accept/reject, value and re-encoding (7M runs, clean).
    Seed with `cargo run --example seed_fuzz_corpus`; run with
    `cd fuzz && cargo +nightly fuzz run <target>`.
12. `benches/parse.rs` (`cargo bench`), over the corpus, fat LTO:

    | | anchovy | `bcs` into owned types |
    |---|---|---|
    | `SenderSignedData`, mean 1,047 bytes | 788 ns, 1 alloc | 2.0 µs, 43 allocs |
    | `CheckpointData`, mean 417 KB | 95 µs, 1 alloc | 290 µs, 5,184 allocs |

    The measure pass is 334 ns of the 788. A profile showed a quarter of
    the time in `core::str::from_utf8` on short identifiers; strings now
    take an ASCII fast path and the build pass skips validation the
    measure pass did. Inlining the one-byte uleb128 case and the small
    reader methods, dropping depth accounting from `Argument` and
    `ObjectArg`, and not pushing repeated package ids took it from 1.1 µs.
    The baseline is generous to the reference: its own types also parse
    signatures at deserialization time, and frees are not timed.
13. `tools/sui-oracle`, a crate outside the workspace and the one place that
    links sui-types, writes what the reference derives from each checkpoint
    as text. `tests/oracle.rs` compares the index and change classes with
    it: all 64 corpus checkpoints match on shared inputs, owned inputs and
    gas, packages, receiving, move calls, input coin reservations,
    withdrawal counts, and created / mutated / unwrapped / deleted /
    unwrapped-then-deleted / wrapped. Build it with `cargo build --release`
    in its directory and run it over `corpus/mainnet/*.chk`.
- `tests/mainnet.rs`: 64 mainnet checkpoints (2,332 transactions, 22,397
  objects) parse; each transaction, effects, events and object re-parses
  alone from its recorded span to an equal view. One checkpoint is checked
  in; `scripts/fetch-mainnet.sh` fetches the rest into `corpus/` (ignored).

## Open

- Arena size per transaction is 1.4x the wire size at the median (p99
  2.2x): `Command` is 80 bytes, `CallArg` 24, and the index copies owned
  refs (73 bytes each). Boxing `MoveCall` would halve `Command` at the cost
  of an indirection on the most common command. Decide whether memory or
  the extra hop matters more once there is a consumer.
- A single-pass mode (guess the arena from the wire length, fall back to
  measure-then-build) would save up to the 334 ns measure pass, but a
  guess big enough for p99 wastes about a wire-length of memory per
  transaction. Not done; the two-pass design is exact.
- The builders mirror the snapshot, so they reject the `Passkey` variants
  of `CompressedSignature` and `PublicKey` that sui and `signature.rs`
  accept (`multisig_with_passkey` in `tests/roundtrip.rs`).
- `MultiSig` parses only through `Message`, which owns its buffer.
  Validation will want to parse signature bytes in place with a scratch
  arena.
- The oracle cannot reach the reference's private gas-payment coin
  reservations, so that part of `coin_reservations` is checked only by
  `tests/tx_index.rs`.
- The `.chk` files in `corpus/` are from one day of mainnet; older shapes
  (effects V1, checkpoint contents V1, genesis, prologue V1 to V3) are
  covered by the builders' round trips and the fuzzers, not by real data.

## Notes

- The snapshot on `origin/main` (4e6ff818cf) has additions over the commit
  `docs/REFERENCE_STRICTNESS.md` was written against: `AllowedProposers`,
  `TransactionExpiration::Validity`, `WithdrawFrom::SenderAllowance`,
  `ForwardingAddressRegistryCreate`, `CommandArgumentError::InvalidTxContext`.
- Container depth is counted only on paths that can reach a `TypeTag`,
  since nothing else recurses. The differential fuzzer found the deepest
  accepted nesting identical to `bcs` in 17 contexts.
- `Ref<'a, T>` is an arena box that is empty in the measure pass; parsers
  must not dereference what they build. Code that has to read parsed values
  back (the index fill) is gated on `Alloc::BUILD` and must make the same
  reservations in both passes. Index counts come only from values that
  exist in both passes: enum variants, wire-backed slices, and
  `Reader::struct_tags()`.
- The index differs from the reference where the reference errors: a
  duplicated input object is kept twice and left for validation to reject.
  It reports owned, shared and package inputs as three slices, so the
  reference's interleaved `input_objects()` order is not kept.
- Effects on mainnet include a change shape the reference lists under no
  accessor: `(NotExist, NotExist, Created)`, created and destroyed in one
  transaction. It is `ChangeKind::Transient`.
- The nightly toolchain, its miri component and `cargo-fuzz` were installed.
