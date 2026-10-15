# The validator pipeline: from submission to certified checkpoint

This design covers everything built so far plus the consensus handler, the
settlement scheduler and the checkpoint builder, all from two building
blocks: **processors** and **waiters**. Consensus stays a black box.
Diagram: `consensus_pipeline.svg`.

## Building blocks

- **Processor.** A dedicated thread owning its state, fed by a bounded work
  queue (`workqueue`), emitting work to other processors' queues. There are
  no locks inside a processor. Several workers may drain one queue when the
  work is independent: voting, and executing transactions whose inputs are
  ready.
- **Waiter.** A processor holding work items until their keys resolve
  (`waiter`):
  - `wait_for(items with keys)` enqueues items;
  - `resolve(keys with values)` resolves keys, each to a value (`notify`
    for keys without one); each waiting item receives its keys' values, so
    whoever does the work need not look them up;
  - ready items go to the processor that does the work.

  A key resolved before anyone waits on it is found by the waiter's
  availability check (a store or table lookup), with its value. So a
  notifier always makes the key visible there first, then resolves it. A
  key no lookup can find must instead be resolved only after its wait is
  queued.
- **Stores and tables.** These hold what availability checks read:
  - the object store: objects, live markers, transactions, effects;
  - the decoded-transaction cache;
  - the generation table (stream generation to version);
  - the chunk table (a settled chunk's ordered effects);
  - the checkpoint store.
- **Black boxes.** Consensus, which takes transactions, asks for votes and
  emits commits; RPC clients; state sync.

Every arrow in the diagram is a queue, except store reads and writes. Every
waiter has three kinds of edge:
- **wait for:** from the processor enqueuing work;
- **notify key:** from each processor making keys available;
- **ready:** to the processor doing the work.

## The consensus handler is a replicated state machine

This is the most important rule in the pipeline. Every validator runs the
same commit handler over the same sequence of commits, and every validator
must reach the same result: the same schedule, ordinals, drops, chunks,
checkpoint numbers and prologue. A difference is a fork.

**The handler may read only two things:**
- its input: the commits consensus hands it, and the epoch's parameters
  (protocol config, committee), which are themselves agreed;
- state it wrote itself while handling earlier commits.

**It must never read anything else.** In particular:
- **The object store or any execution result.** Whether a transaction has
  executed yet, an object's live version, an effects digest. Execution runs
  asynchronously and differs from validator to validator at any moment.
- **Clocks.** Time comes only from commit timestamps.
- **Local configuration, or randomness from the local machine.** Randomness
  seeded from consensus state is fine: it's derived from the input.
- **Iteration order that isn't deterministic.** For example, a `HashMap`
  walked to produce output.

**A bug this rule catches.** The first commit handler deduplicated
transactions against the store's executed transactions. That made the
schedule depend on local, asynchronous state:
- **On a restart:** a transaction that already executed would be skipped,
  shifting every later ordinal on its streams.
- **Without a restart:** whether a repeated inclusion had executed yet was a
  race.

The reference deduplicates by whether the consensus message was processed
before, which is the handler's own state. So does this design. Execution,
not the handler, skips a transaction that has already executed.

**How the rule is enforced.** By construction: the handler has no member
and takes no parameter that reaches state outside consensus. It is built from
its own state alone. It is built for one epoch with the epoch's parameters
(`EpochParams`: what every validator agrees on about the epoch, kept apart
from `EpochState`'s verifier and executor); each call gets a commit and
returns its decisions. The whole commit path, from decoder to handler, is
built with only `EpochParams`. Anything that needs the store, an executor or
a clock happens downstream, in processors the handler never sees.

Two habits keep it deterministic within those inputs:
- **Order:** output order comes from sorted vectors, ordered maps or explicit
  ordering. A hash map or set may be looked up, never iterated to produce
  output.
- **Tests:** two handlers fed the same commits produce identical outputs, and
  so does a handler restarted from its persisted state.

The handler's own state, all derived from consensus alone:
- the last commit handled;
- the transactions processed;
- the owned references consumed;
- the checkpoint sequence state.

Persisting it is the handler writing its own state, read back only by the
handler.

## Wait explicitly, and as late as possible

A pervasive pattern in Sui: look up X; if it doesn't exist, it is guaranteed
to soon, so wait for it. Here that is always a waiter key with an
availability check. Two rules follow.

- **State an ordering as a wait, even when it already holds.** A
  dependency on another component's order is a wait on a key, not an
  assumption that items arrive in that order. Checkpoint `seq` waits for
  `CheckpointBuilt(seq - 1)`, though settlement happens to finish chunks in
  height order. The design stays correct when that order changes: parallel
  settlement, several workers, a restart.
- **Start work as soon as its first input arrives, and wait for the rest
  as late as possible.** Split a stage at the point where it first needs
  the slow dependency. The part before runs at once, for many items in
  flight; only the part after waits. This makes pipelining free.

Where it applies:
- **Checkpoints:** the contents of checkpoint `seq` are built as soon as its
  chunks settle, many checkpoints at a time. Only the summary, which needs
  the previous checkpoint's digest and totals, waits for
  `CheckpointBuilt(seq - 1)`.
- **State hashes:** a checkpoint's ECMH is computed as soon as its effects
  are known. Only folding it into the running root waits for the previous
  root.
- **Settlement:** a chunk's effects are collected and sorted as soon as they
  execute. Only its settlement transactions, whose event indices start at
  the chunk's offset in its checkpoint, wait for the earlier chunks of that
  checkpoint.
- **Execution:** a transaction waits only for the inputs it reads, not for
  its whole commit or the commit before.

## Pipelines

### 1. Submission (built)

As in the reference, a client submits, then waits.

**`submit_transaction`:** RPC handler → `TransactionValidator` →
`SignatureVerifier` → `SubmitChecker` → RPC handler.
- `SubmitChecker` runs the input checks voting runs (owned objects live at
  their versions). It answers each transaction: executed already, rejected,
  or ready to submit, with its `ConsensusTransaction`.
- The RPC handler submits the ready ones through consensus's
  `TransactionClient` (a mirror of consensus-core's), which answers once
  they are in a block: their positions. Each is answered with its
  position.

**`wait_for_effects(digest, position)`:** the RPC handler races two waits
(`select!`), as the reference's handler does:
- the transaction's effects: a wait in the **effects waiter** on
  `Effects(digest)`, which the store answers at once if it executed
  already. Effects can come first even from the same transaction committed
  at another position, submitted through another validator.
- what consensus decided at the position: the **consensus status cache**
  (the reference's `ConsensusTxStatusCache`), which `CommitStage` fills as
  it handles each commit. `Rejected` (by a quorum) and `Dropped` (an owned
  input's lock lost, or invalid inputs) answer rejected; `Finalized` leaves
  the answer to the effects. A position its block never committed expires
  once the committed leader round is `RETENTION_ROUNDS` past it.

The waiter API has no "either of two keys": the race is the async
handler's.

Without consensus (the binary today), `InputChecker` and
`TransactionExecutor` execute submissions directly.

### 2. Voting (built)

Consensus → `BlockVoter` (several workers) → votes back to consensus.

It decodes, applies the block-level rules and the per-transaction votes, then
fills the decoded-transaction cache by `BlockRef`.

### 3. Commit handling

Consensus → `CommitDecoder` → `CommitHandler`.

- **`CommitDecoder` (built):**
  - takes cached transactions and decodes misses;
  - skips quorum-rejected transactions;
  - evicts the cache.

  It routes by kind:
  - user transactions → `CommitHandler`;
  - checkpoint signatures → `CheckpointAggregator`;
  - end of publish, capabilities, JWKs and DKG → an epoch processor (later).
- **`CommitHandler`**, the consensus handler: a replicated state machine
  (see above). It holds no handle to the store.
  1. **Deduplicate** against the transactions it processed in earlier commits
     (its own state), never against the store.
  2. **Owned-object locks:** a set of the owned references consumed in this
     epoch. It's built from consensus order alone, not the store. A
     transaction whose owned input was taken by an earlier one in consensus
     order is dropped.
  3. **Schedule** (built): the prologue first, the protocol's order,
     randomness last, ordinals for shared accesses.
  4. **Chunk** the schedule into checkpoint chunks of at most
     `max_transactions_per_checkpoint`. Each chunk gets:
     - a height;
     - an `AccumulatorSettlement` placeholder, an exclusive writer of the
       accumulator root. So the root is a stream like any other, with one
       generation per chunk.

     Assign checkpoint sequence numbers as the reference's
     `CheckpointQueue` does: flush by count and by
     `min_checkpoint_interval_ms`.
  5. **Build the prologue transaction** and its digest (only cancelled
     transactions, with marker versions), then fill in the schedule's
     prologue entry.

  It emits:
  - each chunk's transactions → `VersionResolver`;
  - each chunk's settlement work → `SettlementScheduler`;
  - each pending checkpoint (its sequence number, chunk heights and
    timestamp) → the **checkpoint waiter**.

#### Why ordinals, not exact versions

The reference assigns each shared input an exact version in the handler.
Ordinals defer that to execution, which simplifies three things:
- **The handler stays pure.** The reference keeps a per-epoch table of each
  object's next version:
  - The first time an object appears in an epoch, the handler reads the
    object's live version from the store
    (`get_or_init_next_object_versions`). That puts a storage read on the
    sequential path and breaks the rule above.
  - The table is persisted only after a checkpoint executes (the consensus
    quarantine), so that replay after a crash finds a table consistent with
    the commits it replays.

  Ordinals are a function of the commit alone. Replay always gives the same
  ones, and there is nothing to persist.
- **Deleted shared objects can't leave a transaction waiting forever.** In
  the reference, a transaction may be assigned version `v` of an object an
  earlier transaction deleted, and `(id, v)` never appears. So deletion
  writes `ConsensusStreamEnded` markers, smeared at the deleter's version for
  every stream it touched, and input loading looks for them. Here the
  deleting writer still produces its generation, recorded as "stream ended",
  and its readers resolve to that.
- **The accumulator root needs no lookups.** The root is a stream with one
  exclusive writer per chunk, so a chunk's root generation follows from its
  height.

The costs:
- **An extra layer:** the resolver, the generation table, and a second key
  type. With exact versions, shared inputs would use `ObjectVersion` like
  owned ones.
- **A harder restart:** the generation table must be rebuilt (see State and
  recovery).

### 4. Execution

`VersionResolver` → **input waiter** → `Executor` (several workers).

- **`VersionResolver`**, in commit order, turns each transaction's inputs
  into keys.
  - Owned, immutable and receiving objects, and packages:
    `ObjectVersion(id, version)`.
  - A shared access at `(round, k)`:
    - `k > 0`: `Generation(stream, round, k)`;
    - `k = 0`: the last generation of the stream that the resolver has
      scheduled in an earlier round, or `StreamLive(stream)` if it has
      scheduled none (the stream as the store holds it).

    This is the resolver's only state: each stream's last scheduled
    generation, in memory, rebuildable from the store.
  - A transaction that withdraws funds also waits on `Reserved(digest)`.
    The resolver queues that wait, then sends the reservations to
    `FundsReserver` (see "Funds reservation"), which resolves the key to
    how they resolved through the same queue. The transaction waits for its
    inputs and its reservations at once.
- **The input waiter** resolves each key to a value, which the executor
  executes from: an object (`ObjectVersion`, `Package`), a generation with
  its object, or a reservation outcome. Its availability check reads the
  object store and the generation table.
- **`Executor`** executes a ready transaction at exact versions read from the
  generation table, then commits its outputs to the store. It then:
  1. records each generation it produced: the produced version for each
     stream it wrote exclusively, or the unchanged version if it was dropped
     or cancelled;
  2. notifies the input waiter: `ObjectVersion` of each output and
     `Generation(stream, round, k + 1)` of each exclusive write;
  3. notifies the effects waiter: `Effects(digest)`.

  Because waits follow the ordinals, workers never conflict.

### 5. Settlement

`SettlementScheduler`, one chunk at a time, in height order.

1. Wait in the **effects waiter** for the chunk's transactions'
   `Effects(digest)`.
2. Order them causally, as the checkpoint builder will: `CausalOrder`, the
   prologue first.
3. Build the settlement transactions (the root as a non-exclusive write) and
   the barrier (the root as an exclusive write). Send them to the input
   waiter on the root's generation keys.
4. Wait for the settlement and barrier effects. Then:
   - write the chunk's ordered effects, settlements and barrier last, to the
     chunk table;
   - notify `ChunkSettled(height)` to the checkpoint waiter;
   - after the last chunk of a commit, notify `Settled` to `FundsReserver`.

#### Funds reservation

`FundsReserver` resolves a transaction's reservations, one per account it
withdraws from, each to `Fulfilled` or `Cancelled`. The transaction waits in
the input waiter on `Reserved(digest)`, notified once all of them have
resolved; it runs only if all were fulfilled. `FundsReserver`
never lets the reservations fulfilled against a balance sum to more than that
balance, and it fulfils a reservation as soon as that is certain, without
waiting for settlement.

**Versions.** Accumulator versions are counted in commits. Version `v` is the
accumulator state once every chunk of every commit before commit `v` has
settled. Commit `v`'s transactions reserve against version `v`, and what they
deposit and withdraw settles into version `v + 1`.

For one account, write:

- `S(v)`: its balance at version `v`, known once `v` settles;
- `σ`: the highest settled version;
- `r₁, r₂, …`: the reservations against `v`, in consensus order;
- `F(v)`: the sum of those fulfilled so far;
- `P(v)`: the sum of those not yet resolved;
- `D(v)`: the sum of the deposits seen so far that settle into `v`, made by
  commit `v − 1`'s transactions. Effects give one net write per account per
  transaction: a net deposit counts here, and a net withdrawal counts against
  the transaction's reservation (credited back as `R − U`). Either way the
  transaction's true change to the balance is counted exactly once.
- `C(v)`: the sum of the credits seen so far against `v`'s fulfilled
  reservations. A reservation of `R` whose transaction withdrew only `U`
  credits `R − U`. A transaction that fails or is cancelled withdraws
  nothing, so it credits all of its fulfilled reservations.

**What makes the bounds sound.**

- `S(v + 1) = S(v) + Dall(v + 1) − W(v)`, where `Dall(v + 1) ≥ D(v + 1)`
  (deposits seen are a subset of all deposits).
- `W(v)`, the amount withdrawn at `v`, is at most what its fulfilled
  reservations allow, less what they credited back: `W(v) ≤ F(v) − C(v) +
  P(v)`, and finally `W(v) ≤ F(v) − C(v)`. A reservation is a maximum, and a
  cancelled transaction withdraws nothing.
- `W(v) ≤ S(v)`: see "Safety" below. So `S(v) − W(v) ≥ 0`.

**Bounds.**

    L(v) = U(v) = S(v)                                       v ≤ σ
    L(v) = max(0, L(v−1) − (F(v−1) − C(v−1)) − P(v−1)) + D(v)    v > σ
    U(v) = ∞                                                 v > σ

By induction from the facts above, `L(v) ≤ S(v) ≤ U(v)`:
`S(v) − Dall(v) = S(v−1) − W(v−1)`, which is at least 0 and at least
`L(v−1) − (F(v−1) − C(v−1)) − P(v−1)`. No deposit limit exists, so the upper bound
stays unknown until `v` settles. A version with no reservations and no
deposits leaves `L` unchanged, so only versions with activity need state.

`L` must subtract `P(v − 1)` as well as `F(v − 1)`: a reservation still
waiting at an earlier version may yet be fulfilled. Without that term, take
`S(5) = 100` settled, `A` reserving 80 at version 6 (waiting, `L(6) = 50`),
and `B` reserving 40 at version 7. `B` would be fulfilled against
`L(7) = 50`. Then 6 settles at 100, `A` is fulfilled, `S(7) = 20 < 40`.

**Rules.** These apply to the earliest unresolved reservation `r` against
`(account, v)`. A later reservation against the same account and version is
never resolved before an earlier one.

- **R1, fulfil:** if `F(v) + r ≤ L(v)`, then `r` is `Fulfilled`: `F(v) += r`
  and `P(v) −= r`.
- **R2, cancel:** if `v ≤ σ` and `F(v) + r > S(v)`, then `r` is `Cancelled`:
  `P(v) −= r`.
- **R3, wait:** otherwise, and only when `v > σ`.

Below the lower bound, R1 schedules. Above the exact balance, R2 cancels, so
cancellation happens only once `v` has settled.

**Safety.** `F(v) ≤ S(v)` always. R1 keeps `F(v) ≤ L(v) ≤ S(v)`, and R2 adds
nothing.

**Determinism.** The outcome is the reference's naive rule: in consensus
order, fulfil `r` if and only if `F_before(r) + r ≤ S(v)`.

- R1 resolves only the earliest unresolved reservation, so `F(v)` is then
  exactly `F_before(r)`. Since `L(v) ≤ S(v)`, R1 fulfils only what the naive
  rule fulfils.
- R2 is the naive rule itself, with `S(v)` known.

So no outcome depends on when deposits are seen or settlements arrive. The
reference's eager scheduler gets the same outcomes, later: it counts no
deposit before settlement.

A transaction is `Cancelled` if any of its reservations is. Its fulfilled
reservations stay in `F`, as in the reference, and are credited in full.

Credits never apply at their own version: there R1 must keep counting
declared maxima, as the naive rule does. They raise `L` only from `v + 1`
on, so they change when outcomes are known, never what they are.

**Events.**

- **`Reserve(account, r, v)`** appends `r` to `(account, v)`'s queue, then
  applies the rules there. Reservations arrive in version order: all of
  `v`'s before any of `v + 1`'s. They are kept in arrival order (consensus
  order), so a reservation key `(account, r, v)` need not be unique.
- **`Deposit(account, d, v)`** is sent by `Executor` once a transaction of
  commit `v − 1` deposits `d`. If `v > σ`, then `D(v) += d`, raising `L` at
  `v` and at every later unsettled version. The rules are then reapplied
  there.
- **`Credit(account, c, v)`** is sent by `Executor` once a fulfilled
  transaction of commit `v` executes, for each account with `c = R − U > 0`.
  `FundsReserver` sends it to itself for each fulfilled reservation of a
  cancelled transaction. If `v ≥ σ`, then `C(v) += c`, raising `L` from
  `v + 1` on. The rules are then reapplied there.
- **`Settled(balances, v)`**, with `v = σ + 1`, is sent by
  `SettlementScheduler` after the last chunk of commit `v − 1` settles.
  - It sets `σ = v` and `S(v)`: given for each account the settlement
    changed, and `S(v − 1)` for the others.
  - It applies the rules at `v` until `v`'s queue is empty; R1 or R2 always
    applies.
  - It reapplies the rules at later versions. `L` there only rose: `S(v)` is
    at least the old `L(v)`, and `P(v)` is now 0.

**An account first seen** needs `S(σ)`.
- Every settlement transaction of a version reads the root at one
  generation, and has no other input object. So each account write at
  version `σ` has version `root(σ)`, the root's version at `σ`, and every
  later one is above it.
- The store's MVCC read of the account at or before `root(σ)` is therefore
  exactly `S(σ)`, whatever settlement has committed since.
- `Settled` carries `root(v)`.

Unlike the reference, the checkpoint builder doesn't rebuild the settlement
transactions to learn their digests: it reads the chunk table.

### 6. Checkpointing

**Checkpoint waiter** → `ContentsBuilder` → **sequence waiter** →
`SummaryBuilder`; signatures → consensus; **summary waiter** →
`CheckpointAggregator` → state sync.

Building a checkpoint is split at its first need for the previous one.

1. **A pending checkpoint** waits in the checkpoint waiter for
   `ChunkSettled(height)` of each of its chunks.
2. **`ContentsBuilder`** (several workers, many checkpoints in flight) does
   everything that needs only this checkpoint, from the chunk table's
   ordered effects:
   - the contents (execution digests and user signatures) and their
     digest;
   - this checkpoint's gas cost summary;
   - the artifacts commitment.

   It sends the effects to `StateHasher`, then waits in the sequence waiter
   for `CheckpointBuilt(seq - 1)`.
3. **`SummaryBuilder`** does what needs the previous checkpoint:
   - the previous digest;
   - the network total;
   - the epoch's rolling gas cost;
   - the summary, with the version-specific data.

   It writes the checkpoint store, then:
   - notifies `CheckpointBuilt(seq)` to the sequence waiter, releasing
     `seq + 1`, and `LocalSummary(seq)` to the summary waiter;
   - signs, and submits the signature to consensus.
4. **`StateHasher`** computes each checkpoint's ECMH as its effects arrive.
   Only folding it into the running root waits for the previous root.
5. **`CheckpointAggregator`** gets signatures from `CommitDecoder` (and from
   the voter's batch verification).
   - Signatures for a checkpoint not built here yet wait in the summary
     waiter on `LocalSummary(seq)`.
   - At quorum on the local digest, it stores the certified checkpoint and
     hands it to state sync.

## Waiter keys

| waiter | key | waited for by | notified by | availability check |
|---|---|---|---|---|
| input | `ObjectVersion(id, version)` | `VersionResolver` | `Executor` | object store has it |
| input | `Generation(stream, round, k)` | `VersionResolver`, `SettlementScheduler` | `Executor` | generation table has it |
| input | `StreamLive(stream)` | `VersionResolver` | (never; always available) | object store |
| input | `Reserved(digest)` | `VersionResolver` | `FundsReserver` | never: resolved only after its wait is queued |
| effects | `Effects(digest)` | RPC submission, `SettlementScheduler` | `Executor` | executed effects in the store |
| (`FundsReserver`) | `Reservation(account, amount, v)`, resolving `Fulfilled` or `Cancelled` | `VersionResolver` | `Settle::Deposit` and `Settle::Credit` from `Executor`, `Settle::Settled` from `SettlementScheduler` | the rules of "Funds reservation" |
| checkpoint | `ChunkSettled(height)` | `CommitHandler` (pending checkpoint) | `SettlementScheduler` | chunk table has it |
| sequence | `CheckpointBuilt(seq)` | `ContentsBuilder` | `SummaryBuilder` | checkpoint store has it |
| summary | `LocalSummary(seq)` | `CheckpointAggregator` | `SummaryBuilder` | checkpoint store has it |

## What must match the reference

These are byte-for-byte or order-for-order:
- **The schedule:** order, the prologue, and its digest.
- **Versions:** the exact versions the ordinals resolve to, so effects match.
- **Checkpoints:** the chunking, heights and sequence numbers.
- **Settlement:** its transactions' contents and event indices, which come
  from the causal order and the chunk's offset in its checkpoint.
- **Checkpoint contents:** order, signatures, summary fields and the
  artifacts commitment.

Everything else is internal: processors, waiters, ordinals, the tables.

## State and recovery

In memory for now; each piece rebuildable or persistable later:
- the commit handler's last commit, consumed owned references, checkpoint
  sequence state and processed digests;
- the resolver's last generation per stream;
- the generation and chunk tables.

On restart, consensus replays commits after the last one whose checkpoints
were built, as in the reference. The handler gives the same schedule and
ordinals as before the crash. Some of those transactions executed already,
and a reader may not have run while a later commit's writer moved its object
on. So the generation table is rebuilt from what execution recorded:
- **An executed writer:** execution skips the transaction, but re-publishes
  the generation it produced, from its stored effects.
- **A stream's starting point:** the input version recorded in the effects of
  its first replayed writer that executed. If no writer of the stream in the
  replayed commits executed, the live object is the starting point.

Old object versions stay in the store until their checkpoint is executed and
pruning allows, so a reader resolved to a historical generation still finds
its object.

## Not covered yet

- End of epoch: end of publish, advance epoch, and reconfiguration.
- Randomness (DKG, randomness state updates).
- Congestion control (deferral, cancellation).
- JWKs and capabilities.
- State sync and the checkpoint executor for full nodes.

## Implementation order

1. Take the store out of `CommitHandler` and deduplicate by its own state;
   then owned-object locks, chunking and the checkpoint queue; the prologue
   transaction.
2. `VersionResolver`, the input waiter, the generation table, and an
   `Executor` pool executing at resolved versions. Commit handling stops
   executing serially.
3. The effects waiter. Submission goes through consensus and waits for
   effects.
4. `SettlementScheduler`, the chunk table, `FundsReserver` and the funds
   waiter.
5. `ContentsBuilder` and `SummaryBuilder` with the checkpoint and sequence
   waiters, then `StateHasher`.
6. `CheckpointAggregator` with the summary waiter.
