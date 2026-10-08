# Phase 7: objects, input checks and execution

Requested after phase 6 (not in `PRD.md`): an object store, a processor
doing the stateful input checks (later also used for consensus voting), a
type state for transactions whose inputs passed, a processor executing
transactions with `sui-execution`, serial execution against the latest
object versions, and a minimal genesis. One node, no consensus.

Decisions (asked): the input checks are our own, over zero-copy object
views, differential-tested against `sui-transaction-checks`; genesis is
objects only; `SubmitTransaction` answers with the effects.

## Overview

- `crates/store`: a Tidehunter database after `AuthorityPerpetualTables`,
  only the tables used now. Values are the reference's BCS, read back as
  our zero-copy views (`messages::object::Object`, effects, events).
- `crates/execution`: everything that links `sui-execution` and
  `sui-types`: the executor, the store seen as a `BackingStore`, and the
  genesis builder. The rest of anchovy keeps to its own types; this crate
  converts at the boundary (BCS bytes in, BCS bytes out).
- `validator`: two more stages and states. `Request<Verified>` →
  `InputChecker` → transactions in state `InputsChecked`, with their
  loaded inputs → `TransactionExecutor` → answer with effects.

## Store

Tidehunter, used directly (not through `typed-store`, which would pull in
RocksDB). Key spaces, after `AuthorityPerpetualTables`:

| key space | key | value |
|---|---|---|
| `objects` | object id ‖ version (big-endian) | `Object` BCS |
| `live_objects` | object id | version ‖ digest of its live version |
| `transactions` | transaction digest | `SenderSignedData` BCS |
| `effects` | effects digest | `TransactionEffects` BCS |
| `executed_effects` | transaction digest | effects digest |
| `events` | transaction digest | `TransactionEvents` BCS |

- `live_objects` stands in for finding an object's latest version (the
  reference scans `objects` backwards per id); it is removed when the
  object is deleted or wrapped. "Available for consumption" is: live, at
  the requested version.
- A transaction's outputs (new and deleted objects, live markers,
  transaction, effects, events, executed effects) commit in one write
  batch.
- Not now: pruning, checkpoints, epochs, indexes, the per-epoch tables.

## Genesis

A minimal builder in `execution`, not `sui-genesis-builder`:

- The framework packages (`sui-framework`'s compiled `BuiltInFramework`).
- Gas coins for configured `(address, amount)` pairs.
- The system objects execution needs at the configured protocol version,
  made by their framework `create` functions in one genesis-mode
  transaction (`Executor::update_genesis_state`), as the real genesis
  makes them: the Clock, and whatever else execution turns out to need
  (the accumulator root, for one, when address balances are on). No
  `SuiSystemState`, validators or staking: epoch facts stay on the command
  line.
- Written into an empty store at first start.

## Input checks (`InputsChecked`)

Our own implementation of `sui-transaction-checks::check_transaction_input`
over objects read from the store, in its order:

1. Load the inputs: owned and immutable objects at their given version,
   shared objects (latest), packages, gas coins; receiving objects apart.
2. Gas: the gas coins are coins, owned by the gas owner, with balance for
   the budget.
3. Objects: none mutable used twice; at least one input; each object is
   what its input kind says (package or not, version and digest, owner:
   the sender's or, for gas, the gas owner's; shared as declared; no
   child objects; no party objects).
4. Replay protection; receiving objects.

No owned-object locks yet: they come with voting. Until then two
transactions can both pass input checks against the same owned object
version before either executes; execution catches it (below).

`validation::ErrorKind` gains the reference's stateful kinds. Not now:
checks of packages to be published (bytecode verification at signing;
execution verifies anyway), allowance inputs, gasless inputs, the deny
list.

`InputsChecked` is minted in `validator::checks` like `Valid` and
`Verified`, so nothing reaches execution unchecked. A transaction already
executed is answered from the store instead (the reference does the same).

## Execution

- `TransactionExecutor` executes a request's transactions one at a time,
  each committed before the next, so each reads the latest version of
  every object. Shared objects: the latest version. Owned objects: a
  debug assertion that the live version and digest are the ones given;
  in a release build a mismatch (the race above, until locks) refuses the
  transaction rather than execute it against a consumed version.
- `sui-execution` with the real `sui-protocol-config` (anchovy's copy is
  for validation; `EpochState` holds both, for the same version and
  chain), `CheckedInputObjects` built from the loaded inputs, a
  `SuiGasStatus` for the budget, price and reference gas price.
- Effects, events and output objects go to the store and back to the
  handler, which answers `SubmitTransaction` with the reference's
  "executed" result (effects digest, and the effects, events and objects).

## Testing

- Store: round trips, live markers, atomic commits, reopening.
- Genesis: the objects exist, packages load, a gas coin pays for a
  transfer.
- Input checks: differential against `sui-transaction-checks` (a test-only
  dependency) over crafted object sets and transactions: every check's
  pass and fail sides.
- Execution: transfers, a Move call, a failed transaction (gas still
  charged), serial dependence (a transaction using another's outputs),
  a double spend refused, a resubmission answered from the store.
- End to end over gRPC, and `tools/validator-client` with a funded account.
- Benchmark: per-stage cost.

## Steps

1. `store`: Tidehunter key spaces, reads as views, atomic commits.
2. `execution`: executor wiring, `BackingStore`, genesis builder.
3. `InputsChecked` and the input checker, with the differential tests.
4. The executor processor, the pipeline, the executed response.
5. Command line (database path, genesis allocations), validator-client,
   benchmark.
