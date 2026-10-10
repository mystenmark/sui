# Phase 8: an executor over zero-copy types

Requested after phase 7 (not in `PRD.md`): rewrite `sui-execution`'s
latest version onto anchovy's types, so execution reads transactions and
objects as `messages` views and builds its outputs with `messages::build`,
without converting to and from `sui-types` at the boundary.

Guidelines (from the request):

- Rewrite, do not redesign. Follow `sui-execution/latest` closely, so the
  new code reads line for line like the old: same modules, functions,
  order of checks, names. Differences are in types, ownership and
  allocation only.
- Only the static PTB executor (`static_programmable_transactions`) is
  ported; the legacy PTB executor and the paths for older protocol
  behaviour are left out.
- The arena rule (`PRD.md`): per-transaction temporaries live in the
  transaction's `Bump`, sized with `with_capacity` where the size is known.
- Minimal clones: borrow views, move owned values, clone only `Arc`s.

Decisions (asked): the natives' object runtime is rewritten too; every
transaction kind the latest engine runs is supported (user and system
PTBs, genesis, consensus commit prologue, end of epoch with advance-epoch,
safe mode and system package upgrades, randomness and authenticator state
updates); phase 7 merges first.

## What stays

- The Move VM and its types (`move-vm-runtime`, `move-vm-types`,
  `move-binary-format`, `move-core-types`), unchanged. Its API takes owned
  `TypeTag`s, `ModuleId`s, `Identifier`s, `Vec<Type>` and `Vec<Value>`;
  those allocations remain, made as late as possible and only where the VM
  is called.
- `sui-verifier` (publish-time bytecode verification) and the Move
  bytecode verifier.
- Pure-computation natives (crypto, hashing, address, types, scratch,
  stdlib): copied verbatim, since about 15 of them read the protocol config
  through the object runtime extension, whose type changes.
- `sui-execution` itself, as the test oracle.

`sui-protocol-config` is used as is: anchovy's copy carries only what
validation reads, and the Move VM's configuration is built from sui's.

`sui-types` may still be linked for leaf utilities that do not carry
transaction or object data (constants, error codes, id derivation, nitro
attestation); transactions, objects, effects, events and execution status
are anchovy's.

## Crates

- `crates/executor`: the port of `sui-adapter/src` (latest), same module
  tree: `execution_engine`, `temporary_store` (+ `invariants`),
  `gas_charger`, `gas_meter`, `adapter`, `execution_mode`, `data_store`,
  `type_layout_resolver`, `error`, `static_programmable_transactions/{env,
  spanned, loading, linkage, typing (verify, invariant_checks), metering,
  execution}`, plus the gas model (`SuiGasStatus` V3, cost tables, storage
  gas) ported from `sui-types`.
- `crates/natives`: the port of `sui-move-natives` (latest): the object
  runtime and the natives that use it (dynamic fields, transfer, object,
  event, accumulator, funds accumulator, config, tx context, package,
  transaction context) rewritten; the rest copied.
- `containers`: an arena `IndexMap`/`IndexSet` (insertion order decides
  written-object and event order, so it must match), and `BTreeSet` if
  missing.
- `messages`: views and builders the executor needs that are missing
  (e.g. `TypeInput`, the system transaction kinds as views, `ExecutionError`
  details), following the existing patterns.

## Types

`messages`' views are `Copy` with public fields and borrowed parts, so the
executor builds them directly in its arena as its in-memory
representation (e.g. `ExecutionErrorKind<'a>`, `Owner<'a>`), and
`messages::fast` writes them once.

- In: `TransactionKind`, `ProgrammableTransaction`, `CallArg`, `Command`,
  `GasData` as views of the transaction's `Message`; input objects as
  `Message<Object>` views from the store. Pure arguments and object
  contents are borrowed slices, deserialized straight into VM values.
- The adapter's `Type` tree (`Rc<Datatype>` with owned `ModuleId`,
  `Identifier`, `Vec<Type>`) becomes arena references with borrowed
  identifiers; `Type` → `TypeTag` conversions, which happen per call,
  transfer and written object today, happen only at VM calls.
- Objects: the executor's `Object<'a>`, `Copy`, holding the view's parts
  (private, behind getters) and, while unchanged, the stored bytes. A
  change makes a new value sharing the contents slice; no field changes
  under bytes that no longer encode it.
- Written objects: metadata (owner, version, type, previous transaction,
  storage rebate) and a contents slice, replacing `Object =
  Arc<ObjectInner>`, whose copy-on-write deep-copies every mutated input
  (owned or shared), contents included. The contents are the bytes the VM
  serialized if Move changed them, else borrowed from the input's stored
  bytes (a version bump changes only metadata). Serialized once, by
  `messages::build`.
- Object digests are a hash of the object's BCS, which is the stored bytes:
  computed from them, never by re-serializing. Inputs have theirs already
  (the owned reference, the store's live marker); a child object read at
  an older version is hashed once, when loaded, before it is deserialized.
- Out: effects (V2), events, written objects and the gas summary built by
  `messages::build` in the arena and serialized once; execution status and
  errors as `messages` types.
- Per-transaction maps (`BTreeMap`, `IndexMap`, `IndexSet`) become arena
  containers. `Rc<RefCell<TxContext>>` stays where the natives share it.

## Execution inputs

Execution takes the transaction plus its loaded inputs; it never loads an input itself.

- **Loader.** The input checker already loads every input object to check it. It hands them on
  with the transaction instead of dropping them: per input object its kind and either the
  object (its stored message), a consensus-stream-ended marker, or a cancellation. These are
  owned messages, so they cross the queue to the execution processor.
- **`ExecutionInputs<'a>`** (executor `inputs.rs`) is built in the execution arena from those
  messages. It holds the transaction view, the input objects as `Object<'a>` keyed by id (stored
  bytes kept, so digests come from them), and what the reference derives from `InputObjects`:
  exclusive mutable inputs, non-exclusive inputs (ids only: the originals are the input objects,
  which nothing changes), stream-ended objects, shared inputs for effects, cancellations,
  dependencies, receiving objects and the lamport version. The derivations are ported from
  `InputObjects`' methods and checked against them. It is read-only during execution.
- **Reads during execution** go through one handle, the `BackingStore<'a>` and
  `ObjectFundsResolver` traits: packages, child objects, received objects, implicitly read
  system objects and config objects, balances. These cannot be known before execution.
- **Temporary store** borrows `&'a ExecutionInputs<'a>` in place of the fields it derived from
  `InputObjects` and the input object map it cloned; its write set, accounting and checks are
  ported as they are.

## Child objects

- The fingerprint that decides whether a loaded child changed is its
  stored bytes (a borrowed slice) with its owner and type, instead of a
  deep copy of its deserialized value compared by `Value::equals`. At the
  end the final value is serialized and the bytes compared: BCS is
  canonical, so equal values under one layout have equal bytes. Modified
  children are serialized for writing anyway; unmodified ones cost about
  what `equals` did. A deliberate departure from line for line, noted in
  the code.
- Each load still deserializes the value and walks the bytes again for
  the UIDs inside (`get_all_uids`); the fully annotated layouts that walk
  needs are cached per type within the transaction. Removing the second
  walk (UIDs reported while deserializing, or a scanner over precomputed
  UID offsets) is a redesign, left for later unless the benchmark calls
  for it.

## Gas model

Ported from `sui-types` for gas model 15 on (no `SuiGasStatus` V2). The
version predicates stay whole: the unmetered `GasStatus` reports version
11, so code consulting them branches as the reference does. Cost tables
are static slices instead of `BTreeMap`s built and cloned per
transaction; the per-object storage report execution never reads is not
kept.

## Left out

Legacy (from the survey): the non-static PTB executor, `mod legacy` in the
engine, legacy gas charging, gas models below 15 (`SuiGasStatus` V2),
`ExecutionResults::V1`, version checks that are constant at the latest
gas model, flags already on at the latest protocol (hard-coded), with
chain-dependent ones (`disable_effects_tx_dependencies`) kept as flags.
Also: dev-inspect (fullnode only), Move tracing, test-scenario natives.

## Testing

Differential against `sui-execution`, byte for byte: the same store state
and transaction through both, comparing effects, events, written objects
and gas summary bytes, and the execution error.

- Crafted transactions: transfers, Move calls, publish and upgrade,
  dynamic fields, shared objects, receiving, events, accumulators and
  address balances, aborts, out of gas, every gas path (smashing, rebates,
  storage overflow), invariant failures.
- System transactions: genesis, consensus commit prologue, randomness and
  authenticator updates, end of epoch with advance epoch, safe mode and a
  system package upgrade.
- Replay: mainnet checkpoints (`scripts/fetch-mainnet.sh`) carry each
  transaction's input and output objects; execute and compare against the
  recorded effects (packages may need fetching).
- Allocation tests (as in phase 6) for the steady state.
- Benchmark against `sui-execution` per transaction kind.

## Steps

1. Containers (arena `IndexMap`/`IndexSet`), missing `messages` views and
   builders; the executor crate skeleton with the gas model, errors,
   execution status and modes.
2. Natives: verbatim copies, then the object runtime and object natives on
   views.
3. Static PTBs: data store, linkage, loading, typing (verify, invariant
   checks), metering, execution; temporary store, gas charger and the
   engine path for user PTBs. Differential harness; crafted user
   transactions agree.
4. System transactions and genesis; their differential tests.
5. Replay against mainnet; anchovy's `execution` crate switches to the new
   executor; allocation tests; benchmark.
