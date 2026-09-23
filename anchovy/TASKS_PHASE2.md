# Phase 2 progress: containers and fast builders

Plan: `IMPLEMENTATION_PLAN_PHASE2.md`. Branch `mlogan-phase2`, to be merged
into `anchovy-main`.

## Status: plan complete

## Done

1. `crates/arena`: `Bump`, a bump allocator implementing `Allocator` from
   both `allocator-api2` 0.2 and 0.4 (the two are distinct traits;
   `hashbrown` and its `Vec` use 0.2, `arena-btreemap` uses 0.4). The first
   chunk is inline and the overflow list allocates lazily, so an arena that
   never overflows is exactly one allocation; `chunks()` reports overflow.
   No `bumpalo`. Miri clean.
2. `crates/containers`: `Vec`, `Box`, `HashMap`/`HashSet` (`hashbrown` with
   `foldhash`), `BTreeMap` (`arena-btreemap`, the standard tree ported to
   stable with allocator support), `SortedMap` (sorted `Vec<(K, V)>`, binary
   search above 16 entries, scan below), and `MessageMap`/`MessageSet`
   whose hasher is the digest's first eight bytes. One test fills every
   container from one arena and checks it never grew. Miri clean, including
   `arena-btreemap`.
3. `messages`: `Digested` names the three `Message` types; `Message<T>` of
   one compares, orders and hashes by digest. `Digest::hash` is one
   `write_u64` of its first eight bytes, the `MessageMap` contract. The
   views keep structural equality, which tests rely on.
4. `messages::fast`: `Writer` (the mirror of `Reader`, including the
   42-variant execution status), `EventsBuilder`, `EffectsBuilder`,
   `ContentsBuilder` (V2), `SummaryBuilder`. Inputs are borrowed for the
   arena's lifetime; `finish` writes once into the arena and hashes the
   bytes with the `"<serde name>::"` prefix. `EffectsBuilder` takes changes
   in id order (a debug assertion) and sorts and deduplicates dependencies
   at the end, the reference's `BTreeSet` order; `unchanged_consensus_objects`
   stay in input order. `tests/fast.rs` rebuilds every effects (2,346),
   event set (1,246), contents and summary (65) in the corpus from its
   parsed view: bytes and digests identical, one arena chunk per checkpoint.
5. `benches/build.rs` (`cargo bench --bench build`): build + serialize +
   hash + drop per item, over the corpus, against the reference's shape
   (`BTreeSet` of dependencies, `BTreeMap` of changes, a scan for the gas
   index, the serde mirror, `bcs::to_bytes` for the bytes and again for the
   hash, as `default_hash` does):

   | | fast builder | reference shape | speed-up |
   |---|---|---|---|
   | `TransactionEffects`, 1.25 KB | 1.6 µs, 1.16 allocs | 2.6 µs, 31.6 allocs | 1.6x |
   | `TransactionEvents` | 1.1 µs, 1.01 allocs | 1.8 µs, 31.2 allocs | 1.7x |
   | `CheckpointContents`, 6.4 KB | 7.2 µs, 1.00 allocs | 9.3 µs, 96.6 allocs | 1.3x |

   About 1 µs per KB of each side is Blake2b-256, which both pay once
   (the reference here hashes one of its two serializations); on build and
   serialization alone the builders are roughly 3x faster. The 1.16 is the
   benchmark's fixed 8 KB arena guess falling short for 16% of effects;
   the capacity heuristic is out of scope by the PRD.

## Open

- `SummaryBuilder` is not benchmarked; a summary is 174 bytes and its cost
  is the hash.
- No `CertifiedCheckpointSummary` builder: the envelope is written by
  whoever signs, in the validation phase.
- The builders take view types (`Owner<'_>`, `TypeTag<'_>`, ...) for nested
  values. Execution will have its own representation of these; the
  `Writer` methods are the seam.
- Parsing a `Built` back needs a `Vec` copy for the `WireBuf`; folding the
  parse arena into the builder's arena is a later step if it matters.
- `MessageMap` relies on a documented contract with the key's `Hash`
  impl; a key type from another crate that hashes differently hits an
  `unreachable!` in debug and a zero hash in release.

## Notes

- `allocator-api2` appears twice in the tree (0.2 and 0.4) by necessity;
  the workspace names the second `allocator-api2-04`.
- Dependencies added: `arena-btreemap` 0.1.2 (the only one not already in
  sui's lockfile), `hashbrown` 0.17, `foldhash` 0.2, `allocator-api2`.
