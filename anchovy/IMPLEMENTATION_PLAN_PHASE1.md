# Anchovy Phase 1: wire format types

Requirements are in `PRD.md`. This plan covers Phase 1 only.

## Overview

A new crate, `anchovy-types`, that parses and builds every type named in
sui's `format__sui.yaml.snap` (a copy is checked in under
`crates/anchovy-types/tests/data/`). Parsing is zero-copy: a parsed message
owns the wire buffer and one arena, and everything else is a reference into
one of the two.

## Goals

1. Deserialization only creates the in-memory representation. It accepts
   every byte string that is well-formed BCS for the format and does no
   semantic validation, because messages also come from trusted sources
   (the database) where redoing validation is wasted work. Everything the
   sui-types `Deserialize` impls check beyond the format is deferred to a
   separate validation step in a later phase; the list is in
   `docs/REFERENCE_STRICTNESS.md` and under "Deferred to validation" below.
2. A parsed message is dropped with two `free` calls.
3. Derived data (shared inputs, owned inputs, receiving objects, packages,
   move calls, effects' created/mutated/deleted sets) is computed at parse
   time and read back as slices.
4. Builder types that serialize to the same format.
5. Fuzzing, with allocation bounded by a constant multiple of input length.
6. Benchmarks on mainnet checkpoints against a serde/`bcs` baseline.

## Non-goals

- The validation step itself, and everything it will own: signature
  verification, zklogin/passkey parsing, roaring bitmap decoding, BLS point
  checks. Those byte strings stay opaque here.
- Transaction validity checks (`validity_check`), digests, hashing.
- JSON / human-readable serde forms.
- Any dependency on sui or MystenLabs crates other than `bcs`.

## Design

### Two buffers

```
Message<T> { wire: WireBuf, arena: Arena, root: NonNull<T::View<'static>> }
```

`WireBuf` is the allocation the network layer read into. `Arena` is one
allocation made during parsing. `Message::get(&self) -> &T::View<'_>`
shortens the lifetime; `Wire::shrink` is a function whose body is `v` and
which therefore only compiles if the view is covariant. `Message` is the
only place in the crate that erases a lifetime.

### Views

Views are ordinary Rust structs and enums with public fields and a lifetime:

```rust
pub struct ProgrammableTransaction<'a> {
    pub inputs: &'a [CallArg<'a>],
    pub commands: &'a [Command<'a>],
}
```

- Byte strings, strings and identifiers are `&'a [u8]` / `&'a str` into the
  wire buffer.
- A sequence whose elements have one fixed wire size is a `&'a [W]` cast
  directly over the wire bytes, where `W` is a `#[repr(C)]`, alignment-1
  struct (`ObjectRef` is 32 + 8 + 1 + 32 bytes; the length byte of the digest
  is validated once). Integers inside are `[u8; N]` little-endian with
  accessor methods. No arena space is used.
- A sequence of variable-size elements is parsed into a native slice in the
  arena.
- Types that are hashed or signed keep `bytes: &'a [u8]`, their exact wire
  span, so digests never need re-serialization.

### One allocation: measure, then build

The arena must be a single buffer holding real references, so its size has
to be known before the first write. Parsing runs twice over the wire bytes:
a measure pass that only adds up sizes, and a build pass that writes. Both
passes are the same source, generic over `trait Alloc`, so they cannot
disagree about structure. The build pass still bounds-checks every
allocation (a mismatch is an error, not undefined behaviour) and asserts
that it used exactly what was measured.

A later optimization, if the benchmark calls for it: guess the arena size
from the wire length, parse once, and fall back to measure-then-build when
the guess is too small.

### Allocation bound

Before reserving `n` elements the parser checks `n * min_wire_size(T) <=
remaining`. Every sequence element type in the format has a minimum wire
size of at least one byte, so arena size is at most `K * wire_len` for a
constant `K` (the largest native-size to minimum-wire-size ratio). The
fuzzer asserts this bound.

### Strictness

Matches `bcs` 0.1.6: minimal uleb128 no larger than `u32::MAX`, sequence
length at most `2^31 - 1`, container depth at most 500 counted the way
`bcs` counts it, `bool` and `Option` tags 0 or 1, map keys strictly
increasing by serialized bytes, UTF-8 strings, no trailing bytes. These are
kept in the parser because they are what makes the encoding canonical:
digests are taken over the retained wire bytes, so a second encoding of the
same value would be a second digest for the same transaction. They cost a
compare each.

Two further checks stay because the representation depends on them: a
digest is 32 bytes and an authority public key is 96 (they are fixed-size
array fields, and fixed-size records are cast straight over the wire), and
`SenderSignedData` holds exactly one transaction.

### Deferred to validation

Parsed messages are unvalidated. A later phase adds the validation step and
a type that only it (or a trusted-source constructor) can produce, in the
manner of `VerifiedTransaction`. Deferred, with the reference location of
each in `docs/REFERENCE_STRICTNESS.md`:

- `GenericSignature` contents: scheme flag, lengths, nested MultiSig /
  zkLogin / passkey decoding and their rules, the legacy MultiSig fallback.
  `MultiSig`, `MultiSigPublicKey`, `CompressedSignature` and `PublicKey`
  (including their `Passkey` variants, which the snapshot omits) get views
  and parsers, but nothing calls them during message parsing.
- `AuthorityQuorumSignInfo`: BLS point validity, roaring bitmap decoding,
  signer count.
- Intent scope, version and app id values.
- Move identifier grammar (`StructTag`, `ModuleId`, `Event`).
- `Owner::Party` permission bits, member ordering and duplicates.
- `AccumulatorValue::EventDigest` being non-empty, `Duration` overflow.

### Builders and the in-tree oracle

`anchovy_types::build` has owned mirror types deriving `serde::Serialize`
and `Deserialize`, serialized with the `bcs` crate. They serve three
purposes:

1. constructing messages;
2. a `serde-reflection` trace of them must equal the checked-in sui
   snapshot, which proves the format matches without depending on sui;
3. `bcs::from_bytes::<build::T>` is an independent parser to fuzz against:
   same accept/reject decision, and `view.to_build() == build` on accept.

### Black-box reference

`tools/sui-oracle` is a standalone crate outside the workspace that depends
on sui-types and only writes test vectors (bytes plus expected derived data
as text). The workspace consumes the vectors as data.

## Implementation order

1. Workspace, lints, `Reader` (uleb128, ints, bytes, str, depth), errors.
2. `Arena`, `Alloc` (measure/build), `WireBuf`, `Message`, `Wire`; Miri.
3. Base types: addresses, digests, `SequenceNumber`, `ObjectRef`,
   `TypeTag`/`StructTag`, `TypeInput`/`StructInput`, identifiers.
4. Transaction: `TransactionData` down through `Command`, system
   transaction kinds, expiration, funds withdrawal, `SenderSignedData`,
   intent, envelope.
5. Transaction index (derived data) built at parse time.
6. Signatures: `GenericSignature` flag dispatch, `MultiSig`,
   `MultiSigPublicKey`, `CompressedSignature`, `PublicKey`.
7. Objects: `Object`, `Data`, `MoveObject`, `MoveObjectType`,
   `MovePackage`, `Owner`.
8. Effects: V1, V2, `ExecutionStatus` and error enums, events; effects
   index.
9. Checkpoints: summary, contents V1/V2, certified summary,
   `FullCheckpointContents`, `CheckpointData`.
10. Builders, snapshot equality test, view-to-builder conversion.
11. Differential tests on mainnet checkpoints; in-tree mutation fuzzer;
    `cargo fuzz` targets with an allocation-counting global allocator.
12. Benchmarks against the serde baseline; optimize.
13. `tools/sui-oracle` vectors for derived data.

## Acceptance criteria

- Snapshot test: traced builder format equals the sui snapshot.
- Every type in the snapshot has a view, a parser and a builder.
- N mainnet checkpoints parse; each re-serializes through the builders to
  the identical bytes.
- Differential fuzzing against the builder types finds no accept/reject or
  value disagreement; no allocation above the bound; Miri clean.
- Parsing a mainnet transaction performs exactly one allocation and is
  faster than the serde baseline, with numbers recorded in
  `TASKS_PHASE1.md`.
