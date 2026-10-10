# Porting sui-execution to anchovy's types

The rules every ported file follows (see `IMPLEMENTATION_PLAN_PHASE8.md`).
The reference is `sui-execution/latest/sui-adapter` (and `sui-move-natives`,
`sui-types` where they are ported); ported files keep its paths.

## Fidelity

- Same modules, functions, names, order of checks and error kinds, so a
  ported file diffs against the reference line by line. Only types,
  ownership and allocation change.
- Do not fix, simplify or restructure the reference's logic, even when it
  looks redundant. A deliberate departure (performance, or a type that
  cannot be expressed the same way) gets a one-line comment saying what
  the reference does.
- Error messages may differ (e.g. `{:?}` for views without `Display`);
  error kinds, and anything that reaches effects, must not.
- Leave out: dev-inspect, Move tracing (`trace_utils`, trace builders),
  the legacy engine and legacy gas, `SuiGasStatus` V2, test-scenario
  natives. Keep protocol-flag branches that the latest protocol can still
  take on some chain.

## Types

| reference | anchovy |
|---|---|
| `ObjectID`, `SuiAddress` | `messages::base::{ObjectId, SuiAddress}` (Copy) |
| `move_core_types::AccountAddress` | unchanged where the VM uses it; `exec_types::base::{object_id, move_address}` convert |
| `SequenceNumber` | `u64` (`messages::base::SequenceNumber`) |
| `ObjectDigest`, `TransactionDigest` | `messages::base::Digest` |
| `ObjectRef` | `exec_types::base::ObjectRef` (a tuple, Copy) |
| `Object` (`Arc<ObjectInner>`) | `exec_types::object::Object<'a>` (Copy; getters; `with_*` to change; `seal` before `digest`) |
| `Owner`, `MoveObjectType`, `TypeTag`, `StructTag` (sui) | `messages` views (Copy, `'a`) |
| move `TypeTag`/`StructTag` from the VM | convert with `exec_types::type_tags` |
| `ExecutionError`, `ExecutionErrorKind` | `exec_types::error` (the kind is the view) |
| transaction types | `messages::transaction` views |
| `Vec`, `BTreeMap`, `BTreeSet`, `IndexMap`, `IndexSet`, `HashMap` | `containers::*` in the transaction's `Bump` |
| `Rc<T>`/`Box<T>` of immutable data | `&'a T` allocated in the arena (`containers::alloc`) |
| `String`/`Identifier` built per transaction | `&'a str` in the arena (`containers::alloc_str`) |
| `sui_protocol_config::ProtocolConfig` | unchanged |
| `ExecutionMetrics`, error sub-status codes, constants | unchanged (`sui-types` leaf utilities) |

## Arena and lifetimes

- One lifetime, `'a`, for everything a transaction borrows: its arena
  (`&'a Bump`) and the messages it read from the store. Types that hold
  arena data take `'a`; functions that allocate take `bump: &'a Bump`.
- Per-transaction temporaries go in the arena (PRD rule), with
  `with_capacity_in` where the size is known.
- Values that cross into the Move VM (`TypeTag`, `ModuleId`, `Identifier`,
  `Vec<Type>`, `Vec<Value>`, `SerializedPackage`) are built at the call and
  owned by the VM's API: that boundary is unchanged.

## Clones

- Arena types are `Copy`; copying them is free. Do not clone owned data the
  reference clones when a borrow or a move does: `obj.owner.clone()` is
  `*obj.owner()`, `ty.clone()` of an arena `Type` is `ty`.
- `Arc` clones stay.
