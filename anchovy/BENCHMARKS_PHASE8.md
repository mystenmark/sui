# Phase 8 profile: execution and commit

`cargo bench -p execution --bench execute [native|reference] [transfer|create]` (`TXS`, default
2000): transactions from distinct funded accounts, executed one at a time over a store and
committed. Profiles: Instruments Time Profiler on the bench binary, summarized with
`scripts/profile_summary.py` (`--under`, `--callers`).

Machine: Apple M4 Max VM, macOS. Thread wakeups go through Mach semaphores, slower than Linux
futexes.

## The whole path, per transaction

Validation numbers from `cargo bench -p validator --bench validate single` (mainnet corpus,
one thread); execution and commit from this benchmark (native, transfer / create).

| stage | µs | allocs |
|---|---|---|
| decode (copy + parse) | 0.6 | 2 |
| validity check | 0.2 | 0 |
| digest (Blake2b) | 0.7 | 0 |
| signature verification (Ed25519) | 28.5 (2.1 when cached) | 0 |
| input checks | not measured separately | |
| execution | 36.9 / 45.3 | 437 / 539 |
| commit | 23.1 / 19.4 | |

Signature verification parallelizes across transactions; execution and commit run one
transaction at a time, so they bound the serial throughput: ~60–65 µs per transaction,
~16k tx/s on one executor thread.

## Per transaction

| executor | workload | execute | commit | allocs (execute) |
|---|---|---|---|---|
| native | transfer | 36.9 µs | 23.1 µs | 437 |
| reference | transfer | 42.5 µs | 18.5 µs | 739 |
| native | create | 45.3 µs | 19.4 µs | 539 |
| reference | create | 53.3 µs | 18.4 µs | 844 |

Native is ~15% faster than sui's executor. Commit is about half of execution and the same
for both (same store code).

## Where execution goes (native)

Shares of samples under the executor, `create` / `transfer`:

| | create | transfer |
|---|---|---|
| package resolution through the VM (`resolve_package`) | 23% | 18% |
| linkage resolution (`add_and_unify`, type linkages) | 24% | 14% |
| reading the clock (`Instant::now`) | 15% | 12% |
| `Context::new` (input values, layouts) | 18% | 19% |
| `Context::finish` (write-out VM, layouts) | 14% | 17% |
| typing and verification | 11% | 16% |
| Move bytecode (`eval::step`) | 4% | — |
| malloc and free | ~19% | ~20% |
| store reads (3 per transaction) | 5.5% | 3.6% |

Overlapping rows: package resolution happens inside linkage resolution and layouts.

### Findings

1. **Package resolution is repeated.** One `create` transaction resolves a package through
   the VM 61 times for 4 distinct packages (0x2 ×26, 0x1 ×15, the called package ×14, 0x3 ×6).
   Each resolution is a VM cache hit, but still allocates a `BTreeSet` and a `BTreeMap`, takes
   SipHash `DashMap` lookups, and starts telemetry timers. The reference does the same (its
   `CachedPackageStore` has no per-transaction memo either).
2. **The clock reads are telemetry timers in package resolution.** `Instant::now` costs ~20 ns
   here, but runs ~200–250 times per transaction; 84% of the samples are under
   `resolve_package` / `load_and_verify_packages`, which time even an empty load. The merged
   telemetry PRs (#26407, #26559) are in; the open #26410 skips recording but not the timers.
3. **Layouts and types are recomputed per transaction**: the gas coin's type and fully annotated
   layout (~7% each in transfers), write-out layouts and VM, and the `defining_ids_in_types`
   invariant check (8.5% in transfers).
4. **A fresh 1 MB arena per transaction** (`native.rs`), returned to the OS each time
   (`mach_vm_reclaim` in the profile).
5. **The bytecode itself is a small share**: the work is in resolving packages, linkage and
   types around the call.

## Where commit goes

The executing thread spends ~22 µs per transaction in `WriteBatch::commit`, of which ~17 µs is
the `semaphore_signal_trap` syscall: each batch unparks every pending-promotion shard it
touches (4 by default, all touched as keys spread over the keyspaces), and the WAL writer
sends its tracker thread a message. The woken threads then spend comparable CPU
(`promote_dirty_pending`, parking). Committing one transaction per batch pays these wakeups per
transaction.

## After the first fixes

A per-transaction memo of the runtime's packages in `CachedPackageStore` (61 runtime lookups
per `create` transaction become 4) and one arena reused across transactions:

| workload | execute before | execute after | allocs before | allocs after |
|---|---|---|---|---|
| transfer | 36.9 µs | 28.1 µs | 437 | 382 |
| create | 45.3 µs | 33.8 µs | 539 | 442 |

Clock reads fell from 15% to 3% of execution, runtime package resolution from 23% to 4%.

What is left of the per-transaction type work is not repeated within a transaction: a
`create` makes 3 runtime layouts, 2 struct type loads, 2 write-out layouts and one write-out VM,
~1–1.5 µs each. Removing them needs caches across transactions (layouts and types by type tag),
which rest on a separate argument: that a type tag's layout does not depend on which version
of its packages the linkage picks, since upgrades preserve struct layouts.

## After the review fixes (Group A)

Sorted-vector maps for the execution inputs and object changes, the natives cost table built
once per epoch, invariant bookkeeping only with the expensive checks, events and removals
carried out of the temporary store instead of rebuilt, and per-transaction memos for type
linkages, input layouts, write-out types and event layouts:

| workload | execute | allocs | reference execute | reference allocs |
|---|---|---|---|---|
| transfer | 22.8 µs | 294 | 42.1 µs | 739 |
| create | 30.4 µs | 397 | 53.6 µs | 844 |

## After the epoch type cache (Group B)

A per-epoch `TypeCache` keeps the VM's layouts and types by the answering VM's linkage table
and the type tag (see `type_cache.rs` for why a hit is never false). Layouts are shared, so a
hit allocates nothing:

| workload | execute | allocs | reference execute | reference allocs |
|---|---|---|---|---|
| transfer | 19.8 µs | 199 | 41.7 µs | 739 |
| create | 25.5 µs | 289 | 52.8 µs | 844 |

Native execution is now ~2.1× the reference's speed with a quarter to a third of its
allocations. Commit is unchanged by this work (18–24 µs per transaction, varying run to run).
