# State sync: design

How anchovy serves certified checkpoints to peers, and how it catches up an
epoch whose consensus is gone. The reference's behaviour is specified in
`docs/state_sync_reference.md` (cited as R*n*); the reconfiguration
reference as RCF R*n*.

Scope:
1. **Serving** fullnodes and lagging sui validators (R6 scenario i). Built.
2. **Catching up** the rest of an epoch that a quorum has left (R6 scenario
   ii, R5.3).

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
last checkpoints can never get them. The reference catches it up by
executing the certified checkpoints it fetches from peers, transaction by
transaction, with versions taken from their effects (R4, R5.3).

### Two modes, never mixed within one run of an epoch

An epoch runs in one of two modes:

- **consensus**, as today;
- **synced**: the rest of the epoch is executed from certified checkpoints
  fetched from peers, without consensus.

A node switches from consensus to synced at a committed checkpoint, by
dropping the consensus pipeline. It never runs both over one epoch at once.

The reference does mix them. A validator executes any certified checkpoint
it has not built itself on the synced path, while consensus runs (R5.1). It
avoids races through a per-transaction guard, and by initialising each
object's next version under a mutex when either path first touches it
(R4.7, R5.2).

Anchovy has neither, by design. Shared inputs resolve through ordinals and
the generation table, which only the executor writes. Executing synced
transactions alongside would mean a second writer of that table, and
keeping the consensus path's generations consistent with outputs it did not
produce. Switching at a committed checkpoint keeps each pipeline the only
writer of its state, and makes the switch an ordinary restart point:
everything past the committed checkpoint is discarded and executed again.

### When to switch

The **checkpoint fetcher** is perpetual and runs on the p2p endpoint. Like
the reference's loop, it polls connected peers' `GetCheckpointAvailability`
every 5 s (R2.3). It then fetches and verifies summaries past the running
epoch's committed checkpoint (R2.6).

The node switches the running epoch to synced when both hold:

- the fetcher has verified the epoch's final checkpoint, the one with
  `end_of_epoch_data`: a quorum has certified the epoch's end;
- the running epoch has committed no checkpoint for `STALL` (30 s).

The second condition keeps an ordinary lag from switching modes: consensus
catches up a validator that is merely behind, as long as the epoch's
commits are served. After a restart, a node starts in consensus mode and
replays as today. If its epoch has ended on the network, it stalls, and the
rule switches it.

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

### The synced epoch

`SyncedEpoch` replaces the consensus pipeline for the rest of the epoch. It
is per epoch: dropping it stops it. In checkpoint order, for each verified
checkpoint after the committed one:

1. **Execute** its transactions one at a time, in contents order, which is
   causal. Each is executed by `Execution::execute_at` with:
   - each consensus object at the input version its effects record
     (R4.3). A cancelled transaction's marker versions come through as
     `Input::Cancelled`. A stream-ended object comes through as
     `Input::Ended`, whose deleting transaction is the one among the
     effects' dependencies that deleted it at that version;
   - the accumulator version taken from the checkpoint's barrier
     settlement (R4.4);
   - `Funds::Insufficient` if and only if the effects' status is
     `InsufficientFundsForWithdraw` (R4.5).

   The effects digest must equal the certified one. Otherwise this
   validator has forked, and it panics (R4.10). The outputs go to the store
   as uncommitted, as the executor's do.
2. **Hash** the checkpoint's effects into the running state hash, as the
   state hasher does.
3. **Commit** the checkpoint with its certified summary: a
   `BuiltCheckpoint` made from the summary, its contents and the totals the
   summary states.

The final checkpoint's change-epoch transaction runs last, after everything
else in the epoch (RCF R3.3). Once it commits, the epoch has ended, and the
node loop reconfigures as it does today. The next epoch starts in consensus
mode. If that epoch has also ended on the network, the rule switches it
too.

Execution is sequential. Parallel execution, as the consensus pipeline
does it, can come later. Contents order makes each transaction's inputs
exist when it runs.

### Switching

1. Drop the consensus pipeline. This stops consensus and joins every
   worker, as at the end of an epoch.
2. `Store::clear_uncommitted`. Outputs past the committed checkpoint are
   dropped, to be executed again from certified contents.
3. Start `SyncedEpoch` from the committed checkpoint.

Nothing is written durably. A restart is an ordinary consensus-mode start.

## Out of scope

- Running as a fullnode, and leaving or joining the committee (RCF R4.19).
- Archive fallback (R2.11), and peer scoring beyond trying the next peer
  (R2.8-R2.9).
- Pushing certified checkpoints (R2.4).
- Mixing the two paths within an epoch (R5.1).

## Tests

- **Serving (built):** a sui full node whose only peer is anchovy syncs
  every checkpoint from it, and executes it alike.
- **Catching up:** anchovy is stopped while the sui validators pass an
  epoch's end, with short epochs. Restarted, it finishes the epoch through
  synced mode, then follows consensus in the next.
- **Unit:** versions, accumulator versions and verdicts derived from
  certified effects, against checkpoints the compat network certified.
