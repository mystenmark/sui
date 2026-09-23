# Phase 2: containers, then fast builders (Phase 1a)

Requirements are in `PRD.md`: the arena rule for temporaries, Phase 2
(containers) and Phase 1a (fast builders). Phase 1a is sequenced after the
containers because the builders are their first real user.

## Overview

Two new crates and additions to `messages`:

- `arena`: a bump allocator, `Bump`, that every container and builder
  allocates from. One allocation up front, one free on drop.
- `containers`: the container types the PRD lists, all taking a `&Bump`.
- `messages`: equality and ordering by digest for the `Message` types, and
  `fast` builders for effects, events, checkpoint contents and summaries
  that build in an arena and serialize once.

## Goals

1. Every temporary that needs dynamic storage lives in a `Bump` and is
   freed with it; no container in the new code allocates from the global
   heap during a message's lifetime.
2. `containers` offers a sorted map, the fastest available hash map, a
   digest-keyed message map, and an allocator-aware `BTreeMap`.
3. A fast builder for `TransactionEffects` (V2), `TransactionEvents`,
   `CheckpointContents` (V2) and `CheckpointSummary`: build, serialize and
   drop with one `malloc` and one `free` when the arena's initial capacity
   suffices; the digest is computed once, over the serialized bytes.
4. Benchmarks for the builders against the reference's construction
   pattern (`BTreeMap`s and `Vec`s from the global heap, then `bcs`).

## Non-goals

- A capacity heuristic for the arena; callers pass a size.
- Execution's own data structures (written objects, loaded runtime objects);
  the effects builder takes what execution has already decided.
- Parsing a built message back with no further allocation. A built
  message's bytes are a `Vec`-compatible buffer, so `Message::parse` works
  on them at the usual cost; folding the parse arena into the builder's
  arena is a later step if it matters.

## Design

### `arena::Bump`

A bump allocator over one buffer, `Bump::with_capacity(bytes)`. Allocation
is an offset bump with alignment; deallocation is a no-op; drop frees the
buffer. If the buffer runs out, a further chunk is allocated (so the arena
never fails), which costs a second free; a counter reports it so tests and
benchmarks can assert one allocation.

`&Bump` implements `Allocator` from `allocator-api2` **0.2** and **0.4**.
The two versions are separate traits: `hashbrown` and
`allocator_api2::vec::Vec` use 0.2, `arena-btreemap` (a port of the standard
`BTreeMap` to stable with allocator support) uses 0.4. Implementing both
on our own arena, rather than depending on `bumpalo`, keeps the whole
container set on one allocator with a dependency tree of `allocator-api2`
(two versions), `hashbrown`, `foldhash`, `equivalent` and
`arena-btreemap`, all already in sui's lockfile except the last.

`messages::arena` stays as it is: it is the measured, exactly sized arena
of a parsed message, a different job.

### `containers`

- `Vec<T> = allocator_api2::vec::Vec<T, &'a Bump>`; `Box` likewise.
- `SortedMap<K, V>`: a `Vec<(K, V)>` sorted by key. Built from an iterator
  (sorted, deduplicated on the way in, last write wins) or by `push` of keys
  in order with a debug assertion; `get` is a binary search, or a linear
  scan below a small length. No insert or remove after building.
- `HashMap<K, V> = hashbrown::HashMap<K, V, foldhash::fast::RandomState,
  &'a Bump>` and `HashSet`. `hashbrown` is the standard library's table and
  the fastest general one available; `foldhash` is its default hasher and
  is what the reference gets from `std` too.
- `MessageMap<T>`: `hashbrown::HashMap<Digest, T, DigestHasher, &Bump>`
  where `DigestHasher` returns the first eight bytes of the digest as the
  hash. Keys are message digests, already uniformly distributed; the map
  never hashes bytes. The PRD accepts that a collision in 64 bits is
  mineable at great cost.
- `BTreeMap<K, V> = arena_btreemap::BTreeMap<K, V, &'a Bump>`.

### `Message` equality and ordering

The three `Message` types (`SenderSignedData`, `TransactionEffects`,
`CheckpointSummary`) get `PartialEq`, `Eq`, `PartialOrd`, `Ord` and `Hash`
on their views and on `Message<T>` by digest only. A trait `Digested` with
`fn digest(&self) -> &Digest` names the types this applies to.

### Fast builders

The reference builds effects from `BTreeSet`s and `BTreeMap`s on the global
heap, then `bcs::to_bytes` for storage and a second full serialization for
each `digest()` call. The builders here:

- Own a `Bump` (or borrow one) and put every intermediate in it.
- Take inputs in the shape execution already has, and produce output in
  the order the reference does, so bytes and digests are identical:
  - `EffectsBuilder`: status, epoch, gas summary, transaction digest,
    lamport version; `dependencies` as a sorted, unique sequence (the
    reference's `BTreeSet` order); `changed_objects` sorted by id, emitted
    by pushing entries in id order (a debug assertion checks it) with the
    gas object's index recorded as it is pushed; `unchanged_consensus_objects`
    in input order; events digest computed from the events builder's bytes.
  - `EventsBuilder`: a sequence of events; `finish` returns bytes and the
    digest.
  - `ContentsBuilder` (V2): one entry per transaction in order, each an
    execution digest pair and its signatures with alias versions.
  - `SummaryBuilder`: the fields in order, `content_digest` from the
    contents builder's digest, commitments and end-of-epoch data optional.
- Serialize straight into the arena as BCS, computing the digest over the
  written bytes once with the `"<serde name>::"` prefix, and return a
  `Built { bytes, digest }` that borrows the arena.
- Write BCS by hand with a small `Writer` (uleb128, integers, bytes) that
  mirrors `Reader`, rather than through serde: no intermediate owned types.

The `build` module's serde mirrors remain the reference oracle: every fast
builder result must equal `bcs::to_bytes` of the equivalent mirror value,
and parse to a view that converts back to it.

## Implementation order

1. `arena`: `Bump`, both `Allocator` impls, chunk fallback, allocation
   counter; unit tests, Miri.
2. `containers`: `Vec`/`Box` aliases, `SortedMap`, `HashMap`/`HashSet`,
   `MessageMap` with `DigestHasher`, `BTreeMap`; tests that each lives in
   the arena (allocation counter stays at one).
3. `messages`: `Digested`, digest-based `Eq`/`Ord`/`Hash` on the three
   `Message` types.
4. `messages::fast`: `Writer`; `EventsBuilder`; `EffectsBuilder`;
   `ContentsBuilder`; `SummaryBuilder`. Each checked against the serde
   mirror and the view parser, including the digest.
5. Benchmarks: build + serialize + drop for effects, events, contents and
   summary, against the reference-shaped construction through the mirrors
   and `bcs`; allocation counts as columns.
6. Optimize from the profile.

## Acceptance criteria

- `containers` compiles on stable with no `bumpalo` and no nightly features.
- Each container's allocations come from the `Bump`: the arena's chunk
  count stays at one across a test that fills every container type.
- Every fast builder output equals the mirror's `bcs` bytes for the same
  value, and its digest equals the parsed message's, for a
  variant-covering set of values and for every effects and checkpoint in
  the mainnet corpus rebuilt from its parsed view.
- A build-serialize-drop round trip of a mainnet-sized effects is one
  `malloc` and one `free`, and faster than the reference-shaped path, with
  numbers in `TASKS_PHASE2.md`.
