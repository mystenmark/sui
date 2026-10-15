# Reconfiguration: design

How anchovy ends an epoch and starts the next. The reference's behavior is
specified in `docs/reconfiguration_reference.md` (cited here as R*n*, E*n*,
I*n*). The aim the reference lacks: a clear lifetime for every component,
and a startup and reconfiguration process that reads top to bottom.

## Goals

- Every component is either **perpetual** or **per epoch**, and a
  per-epoch component's lifetime is a Rust value's: dropping the epoch
  stops and joins everything in it. Nothing outlives its epoch, so nothing
  needs the reference's `within_alive_epoch`, `EpochEnded` errors or
  `release_db_handles` (R4.9, I1, bugs in §6.2).
- One loop builds an epoch, runs it until its final checkpoint is
  committed, drops it, and reconfigures.
- One durable commit point says "now in epoch N+1" (E12). Everything
  before it is idempotent, so any crash is ordinary recovery.

Out of scope for now:
- Deciding the end of the epoch correctly: deferred transactions,
  randomness, the close deadline (R1). A simple timestamp rule stands in.
- Protocol upgrades: no capability votes (R2.9, R2.10). The next epoch
  keeps the protocol version and system packages. Its parameters are still
  read from the system state, so upgrades and committee changes will need
  no change to how an epoch is built.
- Joining or leaving the committee (R4.19): this node is assumed to stay a
  validator.
- Executing checkpoints synced from peers.
- Backpressure.

## Lifetimes

| Perpetual | Per epoch |
|---|---|
| `Store` (database and writeback cache) | `EpochState`, `EpochParams`, the committee |
| the gRPC server | consensus |
| state sync | the pipeline: every processor, waiter and table |
| `CurrentEpoch`, the handle to the running epoch | |

A per-epoch component gets what it needs at construction: the epoch's
`Arc<EpochState>` and `Arc<EpochParams>`, and the perpetual services it
uses (the store, the queue to state sync). It never looks up "the current
epoch" (lesson 1 of R§6.8): it only ever sees its own.

Per-epoch on-disk state is keyed by epoch: the start-version table
already is.

## The node loop

```rust
let store = Store::open(path)?;                   // perpetual
let current = CurrentEpoch::new();
let state_sync = StateSync::start(current.clone());
let server = grpc::serve(listener, current.clone());

loop {
    let start = EpochStartRecord::load(&store);   // the durable commit point
    if start.final_checkpoint_committed(&store) { // a crash after the final
        reconfigure(&store, &start);              // commit, before the record
        continue;                                 // (R4.23)
    }
    let epoch = Arc::new(EpochState::new(&start));
    let consensus = consensus.start(&epoch);      // per epoch
    let pipeline = Pipeline::start(&store, &epoch, consensus, state_sync.queue());
    current.set(pipeline.handle());
    pipeline.final_checkpoint_committed().await;
    current.clear();                              // new requests wait
    drop(pipeline);                               // joins every worker
    reconfigure(&store, &start);
}
```

`reconfigure` reads the system state the final checkpoint committed, and
writes the next epoch's start record (below). Nothing else.

The steps of the reference's reconfiguration (R4.10) map onto this:
- Draining execution, terminating epoch tasks and dropping consensus
  (steps 3 and 4, R4.6) is `drop(pipeline)`.
- Discarding uncommitted state (step 5): the cache's uncommitted set is
  asserted empty, as phase 12 decided, since every executed transaction is
  in a checkpoint and the final one has committed.
- Swapping the epoch store (step 11) and the execution epoch (step 15):
  the next iteration builds a new `EpochState` and pipeline. No global
  swaps.

## `CurrentEpoch`

A perpetual `tokio::sync::watch` of `Option<Arc<EpochHandle>>`.
`EpochHandle` is what perpetual components need of a running epoch: its
`EpochState`, its submission and effects queues, its consensus client and
its status cache. `current.get().await` returns the running epoch, or
waits while there is none.

- **gRPC.** Each request takes the current epoch once, and uses it
  throughout. A request whose epoch ends while it waits sees its reply
  channel close: the pipeline that would answer is gone. It then awaits
  the next epoch and retries there. A submission retried this way is
  checked against the new epoch, as the reference's retry after
  `wait_for_epoch` does (R4.13). `wait_for_effects` is answered by the new
  epoch's pipeline from the store: the effects were committed with the
  final checkpoint. Clients see a pause, not an error.
- **State sync** receives certified checkpoints from the aggregator
  through a perpetual queue it owns. Each epoch's pipeline gets a sender
  to it. When synced checkpoints are executed, it will push them into the
  current epoch the same way.

## The epoch start record

The single durable commit point (E12), written to `meta` in its own
batch:
- at genesis, with the genesis checkpoint;
- at reconfiguration, after the final checkpoint has committed.

It holds what `EpochState` and the commit handler are built from, all
read from the store after the previous epoch's final checkpoint committed
(E8):
- the epoch, protocol version, reference gas price, epoch start timestamp
  and epoch duration, from the system state object;
- the committee: each validator's protocol key and stake, from the system
  state's active validators;
- the previous epoch's final checkpoint: its sequence number, last chunk
  height and timestamp, which the epoch's first checkpoint follows.

The chain identifier and chain are perpetual, held by the node.

**Startup** reads it to learn the current epoch (R4.21). If the store's
committed checkpoint is that epoch's final one, the crash fell between the
final commit and the record, and the node reconfigures at once without
starting the epoch (R4.23, E13). Otherwise it starts the epoch and replays
its commits from the start, as phase 12 does.

`reconfigure` is idempotent: reading the system state and writing the
record again gives the same record.

## The final checkpoint

**Ending the epoch (stub).** The first commit whose timestamp reaches
`epoch_start_timestamp_ms + epoch_duration_ms` is the epoch's last. Its
last pending checkpoint is marked `last_of_epoch` (E1). The commit handler
schedules nothing from later commits, but their checkpoint signatures
still reach the aggregator (E15). Deferral, randomness and the close
deadline come later.

**The change-epoch transaction** is the final checkpoint's last
transaction (R2.2). Its fields depend on the epoch's executed effects (the
gas totals, R2.3), so it can't be built when the final commit is
scheduled. It is built in two steps:
1. **The commit handler reserves its version assignment.** In the final
   commit, it schedules the change-epoch transaction's shared accesses
   (the system state object, and the accumulator root if it writes it)
   after every other access of the epoch, and puts them in the final
   `PendingCheckpoint`. Versions stay defined by version assignment, as
   for every shared object.
2. **`EpochCloser` builds it**, a per-epoch processor between
   `ContentsBuilder` and `SummaryBuilder` that passes non-final
   checkpoints straight through. For the final checkpoint, everything else
   in the epoch has executed by then, since its chunks have settled. It:
   - builds `EndOfEpochTransaction [ChangeEpoch]`: the epoch's gas totals
     (the previous checkpoint's rolling gas plus this checkpoint's), the
     final checkpoint's timestamp as the next epoch's start (R2.4), the
     current protocol version and no system packages;
   - sends it to the resolver with its reserved accesses, so it executes
     like any system transaction, into the cache;
   - waits for its effects, appends them to the contents, and fills in
     `EndOfEpochData` (R2.14): the next committee and protocol version
     from the system state it wrote, and the epoch's root state hash from
     the state hasher's running root through the final checkpoint.

The reference executes the change-epoch transaction in the builder without
committing it, then again in the checkpoint executor (R2.13, R3.2). Anchovy
needs neither: the cache commits nothing until its checkpoint is
certified. So it executes once, and commits with the final checkpoint like
any transaction. That removes the builder racing state sync (bugs
526418dd77, c70aa33eeb, 3bd67c24ac) as long as only certified checkpoints
commit.

**Committing it.** `CheckpointCommitter` commits the final checkpoint like
any other. Afterwards it signals `final_checkpoint_committed` to the node
loop, and the epoch has nothing left to do.

**A real genesis.** Anchovy's genesis has no system state object, and
`advance_epoch` needs one, with validators and staking pools. Genesis is
built by sui's genesis builder instead, which also gives the first epoch
start record its parameters and committee.

## Testing

- The node test network gains epochs: a genesis with a short epoch
  duration, so that commit timestamps end the epoch.
- An epoch ends, the next starts with the system state's parameters, and
  transactions keep executing and certifying across the boundary, with
  checkpoint numbering continuing.
- gRPC requests sent during reconfiguration wait and succeed. An in-flight
  `wait_for_effects` is answered by the next epoch.
- Crashes before the final checkpoint commits, between it committing and
  the start record, and after the record: each restarts into the right
  epoch.

## Order of work

1. **Lifetimes without an epoch end.** `CurrentEpoch`, gRPC and state sync
   made perpetual and awaiting the current epoch, consensus per epoch, and
   the node loop with the epoch start record. Tests drive one epoch, as
   now.
2. **The final checkpoint.** A genesis with a system state, the timestamp
   stub, the reserved accesses, `EpochCloser`, `EndOfEpochData`, and the
   committer's signal.
3. **Reconfiguration.** `reconfigure`, the loop crossing epochs, and the
   crash tests.

## Decisions

- The timestamp rule is the stub for ending the epoch.
- `EpochCloser` builds the change-epoch transaction, with its versions
  reserved by the commit handler, and the final checkpoint is built whole
  at once. Executing it into the cache is the "execute without
  committing" step. The committer commits it with the certified final
  checkpoint, so it isn't executed a second time. The reference's two
  executions are likely a vestige of the time before its writeback cache.
  A fork shows as a certified checkpoint differing from the locally built
  one, which the checkpoint executor will compare.
- No protocol upgrades or validator-set changes in this phase, but epoch
  parameters are read from the system state regardless.
