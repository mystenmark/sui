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

    | | anchovy: parse + drop | `bcs` into owned types: parse + drop |
    |---|---|---|
    | `SenderSignedData`, mean 1,047 bytes | 490 ns + 32 ns, 1.01 allocs | 1.9 µs + 477 ns, 43 allocs |
    | `CheckpointData`, mean 417 KB | 52 µs + 0.7 µs, 1.02 allocs | 289 µs + 57 µs, 5,184 allocs |

    Drop frees the input buffer on both sides; the exact two-pass parse of
    a signed transaction is 795 ns + 33 ns.

    With digests (below), parse + drop against the unhashed baseline:

    | | anchovy, digests included | `bcs` into owned types, no digests |
    |---|---|---|
    | `SenderSignedData`, 1,047 bytes | 1.2 µs + 32 ns | 1.9 µs + 409 ns |
    | `CheckpointSummary`, 170 bytes | 207 ns + 5 ns | 83 ns + 21 ns |
    | `CheckpointContents`, 6.4 KB | 183 ns + 21 ns | 3.0 µs + 716 ns |
    | `CheckpointData`, 417 KB | 113 µs + 0.7 µs | 280 µs + 56 µs |

    `CheckpointData` is the full download format, a checkpoint plus every
    transaction with effects, events and objects. A checkpoint proper is
    the summary and the contents. Blake2b-256 runs at about 1.3 GB/s here,
    so on the 170-byte summary the hash is most of the time. The baseline
    does not hash, as the reference does not at deserialization time
    either, and the comparison is left unfair to anchovy that way.
14. Digests. The types the reference implements `Message` for, and only
    those, carry a `digest` computed once from the wire span while parsing,
    in the build pass only, and handed out by reference: `SenderSignedData`
    (the field lives on its `TransactionData`), `TransactionEffects` and
    `CheckpointSummary`. `Object`, `TransactionEvents` and
    `CheckpointContents` have a `digest()` method that hashes their wire
    span on demand, as the reference does. The reference's
    digest is Blake2b-256 over `"<serde name>::"` then the BCS bytes
    (`default_hash` via `Signable::write` in `crypto.rs`); intents are a
    separate layer that only applies to what is signed. Confirmed on all
    64 corpus checkpoints: transaction and effects digests equal those in
    the checkpoint contents, events digests those in effects, contents
    digests the summary's `content_digest`, object digests those in
    effects, and the summary digest that of sui-types via the oracle.
    Hashing uses the `blake2` crate at sui's version, the same crate behind
    fastcrypto's `Blake2b256`, without fastcrypto's dependency tree.

    `Message::parse` is single-pass: it reserves an arena guessed from the
    wire size (`Wire::ARENA_GUESS_SIXTEENTHS`, set per type to the mainnet
    p99 of arena over wire, floor 256 bytes) and falls back to the exact
    two-pass parse (`Message::parse_exact`) when the guess is short.
    Fallback rates on the corpus: transactions 0.5%, effects 0.9%,
    checkpoints 1.6%, events and objects 0%. The guess over-allocates by
    37% (transaction data) to 48% (signed transactions) of what is used;
    for objects parsed alone the floor makes it 7x, which only matters if
    something parses objects one at a time.
    Before single-pass mode the same parse took 788 ns, of which the
    measure pass was 334; before that 1.1 µs. A profile had shown a quarter
    of the time in `core::str::from_utf8` on short identifiers, so strings
    take an ASCII fast path and an exact build pass skips validation the
    measure pass did; the one-byte uleb128 case and the small reader
    methods are inlined; `Argument` and `ObjectArg` skip depth accounting
    they cannot affect; repeated package ids are not pushed for sorting.
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

- Arena used per transaction is 1.4x the wire size at the median (p99
  2.2x). Per programmable transaction (1,094 wire bytes, 2,006 arena):
  type tags 434 (`StructTag` is 56 bytes, mostly two `&str`), index copies
  393 (owned refs are 73 bytes each), `MoveCall` bodies 327, `CallArg`s
  261 (24 each), commands 234, arguments 117. Boxing `MoveCall` was tried
  and made it worse (p50 1.40 to 1.51): 4.6 of the 4.9 commands per
  transaction are calls, so a box adds a pointer and padding to nearly
  every command and saves 40 bytes on almost none. The levers that would
  help are storing index owned refs as 8-byte references instead of
  73-byte copies, and packing `StructTag` strings; both trade an
  indirection for memory.
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
