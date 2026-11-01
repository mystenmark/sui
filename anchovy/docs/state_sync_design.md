# State sync: design

How anchovy serves certified checkpoints to peers, and how it catches up an
epoch whose consensus is gone. The reference's behaviour is specified in
`docs/state_sync_reference.md` (cited as R*n*); the reconfiguration
reference as RCF R*n*.

Scope:
1. **Serving** fullnodes and lagging sui validators (R6 scenario i). Built.
2. **Catching up** a lagging consensus, or the rest of an epoch that a
   quorum has left (R6 scenario ii, R5.3).

Running as a fullnode (scenario iii) is out of scope, though the synced
epoch below is most of it.

## Serving (built)

The p2p endpoint registers sui's five `StateSync` routes. Each handler
decodes the request, waits for the running epoch, and queues the query to
that epoch's checkpoint server, a processor that reads the store and
returns the snappy-framed BCS answer (`validator::state_sync`).

- **Summaries** come from the store's committed checkpoints: by sequence
  number, by digest (a new index), or the latest. Genesis's certified
  summary is installed with genesis, so a peer's first query, which checks
  the chain (R2.1), succeeds from the start.
- **Contents** are spliced from the stored contents and the store's
  transactions and effects, in the reference's encoding, V1 or V2 (R1.6),
  found by contents digest (a new index).
- **Availability** is the highest committed checkpoint, and checkpoint 1 as
  the lowest: genesis's transaction is not kept, and every peer has
  genesis.
- **Pushes** are accepted and dropped.

Anchovy does not push its certified checkpoints yet (R2.4). Peers learn
them by polling every 5 s instead.

## Catching up

### The problem

When a quorum reconfigures into epoch N+1, it stops epoch N's consensus. A
validator whose consensus has not yet delivered the commits behind N's
last checkpoints can never get them. Less drastically, a validator whose
consensus lags falls behind the certified chain. The reference executes
certified checkpoints fetched from peers, with versions taken from their
effects (R4, R5.3), alongside consensus (R5.1).

### A validator validates

Executing a certified checkpoint keeps a node from falling behind, but it
takes the quorum's word. A validator must still perform every action the
quorum performed, and notice if the quorum deviated from the protocol.
So:

- **The consensus path always runs** over every commit consensus delivers,
  including funds reservation, whether or not the synced path got there
  first. The reference skips funds scheduling when checkpoint execution
  is ahead (R4.5). Anchovy does not.
- **Whichever path reaches a transaction second verifies it.** The synced
  path checks the stored effects against the certified ones. The
  consensus path checks every decision it made (versions, cancellation,
  the funds verdict, the accumulator version) against the stored effects.
  Any mismatch means the quorum or this node deviated, and it panics.

Only commits consensus never delivers, at an epoch's end, are taken on
the quorum's certificate alone.

### Two front ends, one executor

The synced path is a second front end into the consensus pipeline's input
waiter, executor, effects waiter and committer. Nothing is torn down, and
there is no mode to switch:

- The **checkpoint fetcher** is perpetual and runs on the p2p endpoint. It
  polls peers' `GetCheckpointAvailability` every 5 s (R2.3), and fetches
  and verifies summaries past the committed checkpoint (R2.6).
- The **synced scheduler**, per epoch, takes each verified checkpoint in
  order. Under the chunk lock, it either compares it with the one built
  locally, or fetches its contents and turns its chunks into executor work
  items.
- The **executor** runs consensus and synced items alike.

### The chunk lock

The two paths never work on the same settlement chunk at once. A chunk is
up to `max_transactions_per_checkpoint` of a commit's transactions, then
their settlement transactions, ending with its barrier. A commit past the
limit splits into several chunks. Chunks never span commits, and their
heights count from 1 each epoch. A checkpoint is a run of whole chunks of
at most the limit in all, cut across commits and within them: a split
commit's chunks may land in consecutive checkpoints. So a certified
checkpoint splits into chunks at its barriers, numbered by counting the
epoch's barriers before it. The epoch's change-epoch transaction follows
the last barrier.

Per epoch, a table holds:

- `handled`: the height of the last chunk the commit handler assigned, and
  `cut`: the last checkpoint it cut;
- `synced`: the chunk heights the synced scheduler is executing.

**Consensus.** Handling a commit claims its chunks, exactly: they are
complete once it is handled. The claim never waits. A claimed chunk that
`synced` holds waits in the gate waiter on `ChunkFree` of its height, and
the randomness gate holds every chunk after it, until the synced scheduler
removes it from `synced` and resolves the key.

**Synced**, for checkpoint K:

- **Fast path: K ≤ `cut`.** A checkpoint is cut only once all its chunks
  are assigned, by a commit handled already, so every chunk of K is claimed
  and K will be built without another commit. The scheduler waits for `CheckpointBuilt(K)` and checks
  that the built digest is the certified one (R5.1). It fetches no
  contents and executes nothing. This is the common case.
- **Slow path: K > `cut`.** K is open or unseen, and may never be cut if
  consensus has stopped. The scheduler fetches its contents and splits
  them into chunks. A chunk at or below `handled` is the consensus path's.
  A chunk above `handled` it adds to `synced` and executes. It waits for
  every transaction of K to have effects, by either path, then checks each
  one's against the certified digest, removes K's chunks from `synced`,
  and commits K.

The epoch's last checkpoint takes the same paths. Its change-epoch
transaction follows its last barrier, outside every chunk, so the slow
path runs it after the chunks, at the input versions its effects give;
those order it after the epoch's other writers.

A chunk headed by a randomness state update waits for its round's bytes,
and the randomness gate holds every chunk after it, until they arrive, which may never
happen once the quorum has moved on. So the synced scheduler gives the
signature collector the bytes in each split checkpoint's randomness state
updates, which it verifies against the epoch's DKG public key. A cut
checkpoint that lists rounds and is not built yet has its contents fetched
for its rounds alone: one built already needed none. A claimed chunk then
always settles.

The commit stage still builds each checkpoint the synced path committed,
and the checkpoint store compares the two. So a chunk leaves the chunk
table when its checkpoint is built, not when some checkpoint commits, and
the state hasher accepts a checkpoint hashed again if it hashes alike.

What this buys:

- A transaction is in one chunk, so the two paths never execute one
  concurrently. A path can still find a transaction executed already, by
  the other path earlier or before a restart, but never in flight.
- Neither path waits on a commit that may never arrive.
- A checkpoint is committed once; whichever path comes second compares.
  When the consensus path later builds a checkpoint the synced path
  committed, it compares, as fork detection does now.

The consensus path never skips a chunk the synced path executed. It
catches up behind, checking every transaction as below.

### Verifying (R3)

- **Summaries** chain from the committed checkpoint's certified summary
  (R3.3). Each must have the next sequence number, the previous digest and
  the running epoch, with the epoch checked before anything is looked up
  (R3.10). It must also carry a quorum of the epoch's committee, from the
  epoch record, signing `CheckpointSummary` with the protocol keys (R3.2).
- **Contents** must match the summary's contents digest, rebuilt from the
  effects, and each transaction must match its effects (R3.5). User
  signatures are not checked (R3.6).

Summaries and contents are held in memory, a bounded window ahead of the
execution. After a restart they are fetched again.

### Synced items

A synced item carries the transaction, its certified effects digest, and
everything the consensus path would have decided, taken from the
certificate:

- each consensus object at the input version its effects record (R4.3),
  as an exact `ObjectVersion` wait in the input waiter. It never touches
  the generation table. A cancelled transaction's marker versions come
  through as `Input::Cancelled`. A stream-ended object comes through as
  `Input::Ended`, naming the transaction among the effects' dependencies
  that left the stream ended at that version: the one that took the
  object out, or a later writer. Where the protocol drops effects'
  dependencies, the name reaches no effects, and none is looked up;
- the accumulator version, from the barrier settlement ending its chunk
  (R4.4). It counts commits, so the chunks of a split commit share it,
  even across checkpoints;
- `Funds::Insufficient` if and only if the effects' status is
  `InsufficientFundsForWithdraw` (R4.5). The synced path cannot wait for
  the funds reserver without waiting for consensus.

If the transaction executed already, the executor checks that its stored
effects digest is the certified one. Otherwise it executes it, and the
effects digest must be the certified one (R4.10). Either way, a mismatch
means this node forked, and it panics.

**Start versions.** The consensus path takes generation 0 of a stream from
`EpochStore::start_version`, which falls back to the live object when
nothing in the epoch recorded it. That is right only before anything in
the epoch has written the object. So a synced item records the start
version of each consensus object it writes, from its effects' input
version, before it executes. Every other version the consensus path reads,
it reads by version, and the store keeps every version, so nothing the
synced path writes ahead can disturb it.

Once all of a checkpoint's transactions have executed, the scheduler
hashes its effects into the running state hash and commits it with its
certified summary: a `BuiltCheckpoint` made from the summary, its contents
and the totals the summary states.

### The consensus path, behind

When the executor finds a consensus item's transaction executed already
(by the synced path, or before a restart), it does not execute it. It
checks the stored effects against what the consensus path decided:

- each shared input's version, `Ended`, or cancellation marker, against
  the effects' input versions;
- the funds verdict: `Cancelled` if and only if the effects' status is
  `InsufficientFundsForWithdraw`;
- for a barrier, the root's input version, against the chunk's root
  version, which counts chunks.

A transaction it runs fresh must write each consensus object from its
live version. Otherwise the certified chain went on from that version
without it: a commit the consensus path decides otherwise than the quorum
did, such as a prologue over other transactions.

Then it records generations and funds settlements from the effects, as it
does now. With the verdict checked, `Settle::Deposit` and `Settle::Credit`
from the stored effects are exactly what executing would have produced, so
the funds ledger matches the quorum's.

The funds reserver needs nothing new. It runs on every consensus commit.
It reads a first-seen account's balance at the settled root version by
version, so balances the synced path has written ahead do not disturb it.
`Settle::Settled` comes from the consensus path's settlement scheduler,
whose settlement transactions are found executed and checked like any
other.

The checkpoint builder compares each checkpoint it builds with the one
the synced path committed, as fork detection does now, and commits
nothing.

A commit's consensus output waits in the output waiter on
`CheckpointProgress::BuiltHere(s)` and `Committed(s)`, `s` the last
checkpoint cut, and `OutputCommitter` writes it once both hold. A synced
commit alone is not enough: a commit decided but whose transactions the
executor has not yet checked would be written, and after a crash never
decided again, so its checks would never run. Settlement goes one chunk at
a time in height order, so a checkpoint built here means every chunk
through its last has executed or been checked.

### The end of an epoch

The final checkpoint's change-epoch transaction runs last, after
everything else in the epoch (RCF R3.3). Once its checkpoint commits, by
either path, the committer tells the node loop, which drops the epoch's
pipeline with both front ends and writes the next epoch's record. The
record's root comes from the epoch's state hashes, folded from committed
effects if missing, so the synced path needs no root of its own. The next
committee, protocol version and root are checked against the certified
end-of-epoch data first. A node whose consensus has been left behind
finishes the epoch on the synced path alone. After a restart, replay
checks every transaction executed before the crash, against outputs this
node produced.

## Out of scope

- Running as a fullnode, and leaving or joining the committee (RCF R4.19).
  A fullnode is the synced front end without the consensus one.
- Archive fallback (R2.11), and peer scoring beyond trying the next peer
  (R2.8-R2.9).
- Pushing certified checkpoints (R2.4).
- Several executor workers.

## Tests

- **Serving (built):** a sui full node whose only peer is anchovy syncs
  every checkpoint from it, and executes it alike.
- **Catching up:** anchovy is stopped while the sui validators pass an
  epoch's end, with short epochs. Restarted, it finishes the epoch on the
  synced path, then follows consensus in the next.
- **Lagging:** anchovy's consensus is delayed while the synced path runs
  ahead within an epoch, with funds withdrawals near their balances. The
  consensus path catches up, checks every transaction, and agrees.
- **Deviation:** anchovy's consensus leaves out a transaction the quorum
  certified, which the synced path ran. Once the consensus path reaches
  that commit, it decides it otherwise and aborts.
- **Unit:** versions, accumulator versions and verdicts derived from
  certified effects, against checkpoints the compat network certified.
