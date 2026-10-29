# Randomness: design

Reference: `randomness_reference.md` (R-numbers below are its), with
congestion details in `congestion_control_reference.md` (prefixed CC).

## Goals

Phase 18a: anchovy follows the random beacon without holding shares.
- It decides what sui decides from consensus output alone: DKG's outcome
  and the commit it lands in, randomness rounds, randomness deferral,
  `DkgFailed` cancellation, the randomness congestion tracker, and the
  randomness chunks' heights and checkpoint placement (R3–R5.6).
- It receives the beacon's signatures that sui validators push over
  anemo, verifies them, and executes each round's `RandomnessStateUpdate`.

Phase 18b: anchovy takes part. It deals and confirms in DKG, and
partial-signs and pushes each round (§6). 18b adds a key holder and an anemo
client; nothing of 18a changes.

Dependencies: `fastcrypto-tbls` at the fastcrypto revision already in
use, and `anemo` at sui's revision (approved).

## 1. DKG as an observer

A new `consensus/dkg.rs` holds the reference's `RandomnessManager`
state, observer role only: `Nodes::new_reduced` from the committee
(R3.1–R3.2), the random oracle, a `dkg_v1::Observer`, the processed
messages by party, the used messages, the confirmations, and the status
(`Pending`, `Successful(vss_pk)`, `Failed`).

- **Decode.** The decoder parses each DKG message and confirmation, checks
  that its author is the block author, and runs `Observer::process_message`
  (the NIZK check, R3.7). That check is the expensive part, and it is a
  pure function of the message and the epoch, so it belongs with the
  decoder's other checks, off the commit handler's thread. A message that
  fails it, or fails to decode, is dropped as the reference drops it.
- **Handle.** Per commit, after the round is reserved (§2): per-authority
  dedup for the epoch, `add_message` and `add_confirmation`'s rules, then
  `advance_dkg` on every commit while DKG is pending: merge once
  processed weight reaches t', complete once confirmation weight
  reaches 2t'−1, fail at the first commit with leader round > 3000
  (R3.4–R3.6).
- **Store.** Into the consensus output, in new epoch store spaces: each
  processed message (keyed by party), the used set once merged, each
  confirmation, and the outcome. A restart rebuilds the state from them
  as the reference's `try_new` does (R6.1). The outcome is also written to
  the signature store (§5), which needs `vss_pk`.

DKG traffic after the epoch's last commit is not handled: later commits
are not handled at all.

## 2. Rounds

`CommitHandler::handle` begins with `init_randomness` (R4.1). Pending: no
round. Failed: no round, and `dkg_failed`. Successful: reserve the next round (the epoch's
last commit too, R6.3) if none was
reserved this epoch or the commit's timestamp is 500 ms or more past the
last round's. Both the next round and the last round's timestamp go into
the consensus output. Rounds count from 0 each epoch (R4.2).

## 3. Deferral, cancellation, the randomness tracker

Step 6 of `handle` also loads every `Randomness`-keyed deferral, when the
commit has a round or DKG failed (R4.3). Step 7 splits the ordered
transactions into regular and randomness lists, regular first, as the
reference does. Each randomness transaction is then:
- **cancelled** with `DkgFailed`, if DKG failed: `0x8` reads as
  `RANDOMNESS_UNAVAILABLE` and other consensus inputs as `CANCELLED_READ`
  (R4.5). These go into the prologue's list as congestion cancellations
  do;
- **deferred** under `Randomness { deferred_from_round }`, if the commit
  has no round (R4.4). There is no limit;
- **otherwise** sent through a second `CongestionTracker`. Its budget and
  burst are scaled by `randomness_scalar`, and its debts sit in their own
  space with their own decay (R4.6, CC R1). Today's handling,
  admitted without a check, goes away.

The epoch close already counts every deferral, so randomness deferrals
hold an epoch open until the deadline, as they should (R6.3).

## 4. Two schedules, and their chunks

The schedule gets a second, randomness part: `RandomnessStateUpdate(round)`
when a round was reserved, then the randomness transactions, scheduled or
cancelled (R4.7). The sequencer assigns ordinals over regular, then
randomness, in one pass, so the update writes `0x8` before its readers
(R5.6.3). `Scheduled` gains a `RandomnessStateUpdate(round)` variant.

`CommitStage::chunk` lays out the regular part as today, flushes the
checkpoint queue, and **then** pushes the randomness part's chunks, when
the commit has a round or `DkgFailed` cancellations (R5.6). Those chunks
wait in the queue as the first roots of the next checkpoint. The epoch's
last commit forces a flush before them, and again after them, so they make
the epoch's last checkpoint on their own (R5.6.4). The commit's recorded height is
its last chunk's, whichever part that is. A commit whose randomness chunks
are pending is not `drained`, so its record is not the restart point.
That costs replay, not correctness.

## 5. The bytes

The update's bytes are not known when the commit is decided. Its digest
is part of its chunk and its settlement.

- **Randomness gate.** A new processor between `CommitStage` and the
  resolver and settlement queues. It forwards chunks and settlements in
  height order. At a randomness chunk headed by an update, it waits for
  the round's signature, builds the update (R5.5), fills in its entry and
  its digest, and goes on. Chunks behind it wait, as the reference's
  checkpoint builder waits (R5.8). This costs latency for a round's
  signature. Releasing later chunks past it would need the resolver's
  funds reservations, which follow consensus order, to accept a gap. Not
  worth it now.
- **Signatures.** An anemo server on the validator's p2p address, under
  its network key, with server name `sui-{chain_id}`. It serves one route,
  `sui.Randomness/SendSignatures` (R5.3), and answers others as
  unimplemented. Its handler queues the request with a oneshot, as at
  every async boundary. A processor verifies a full signature against
  `vss_pk.c0()`. Partial signatures are aggregated per round: verified
  against `vss_pk` per peer, combined at t' weight (R5.2). The result is
  `bcs(sig)`.
- **Store.** Each round's bytes go into the epoch store before they are
  released to the gate. Peers stop pushing a round once it is certified,
  so a restart must find the bytes locally (R5.5, gap 7). Signatures that
  arrive before DKG completes locally, or for rounds not reserved yet, are
  kept, up to a window of 50 rounds past the last released one (R5.2).
- **Peers.** Sui validators push only after their own DKG completes, to
  their committee's p2p addresses (R5.3), so anchovy dials nobody in 18a.
- **Rounds missed.** Peers stop pushing a round once they certify it, so a
  round that completed while anchovy was down never arrives: after any
  restart, rounds certified meanwhile are lost. The p2p endpoint asks the
  gate, every second, which round it waits for and which checkpoint the
  update goes into (a queue entry with a oneshot). A round waited for at two
  asks in a row is fetched from a connected member through sui's state sync
  service (`GetCheckpointSummary`, then `GetCheckpointContentsV2`). The
  update's bytes go to the collector as a full signature, verified against
  `c0` like any other. The members are connected already: they push to it.

## 6. Taking part (phase 18b)

- **The party.** `DkgParty`, a processor, holds a `dkg_v1::Party` made from
  the protocol key's secret. When it acts follows the commit handler's
  observer, which every validator runs alike: `CommitStage` sends it each
  message the handler processed (it decrypts its shares), the merge (it
  merges the same messages and submits its confirmation, with its
  complaints), and the completion (it completes, with its shares, which go
  to the collector). At start it deals and submits its message, unless one
  of its messages was sequenced or DKG merged already. Restarted,
  `CommitStage` replays the state the handler restored, then says whether
  its message and confirmation were sequenced: decryption is
  deterministic, so the shares come back from the protocol key alone.
- **Signing.** `CommitStage` sends the collector each round reserved, and
  at start the last 50 reserved, which may lack bytes. The collector
  partial-signs each with this member's shares; its partials count toward
  the round's like a peer's.
- **Pushing.** Every 5 s the p2p endpoint asks the collector what to push
  (a queue entry with a oneshot): this member's partials of each round
  without bytes, or the full signature once known, for rounds down to 20
  below the lowest without bytes. It sends them to every other member over
  `sui.Randomness/SendSignatures`, dialing a member's p2p address, from the
  system state, if not connected.

## 7. Compat

- The beacon scenarios run the usual three validators. Reduced, the sui
  validators weigh 350 of 525, short of the 351 confirmations need, so DKG
  completes only with anchovy's confirmation. The DKG failure scenario
  times DKG out at round 1, before any confirmation can be committed.
- consensus-glue's four anchovy validators run their p2p endpoints: they
  complete DKG and sign rounds among themselves.
- The sui validators are a quorum without anchovy, and anchovy cannot
  catch up an epoch they left. So a compat test claims four of nextest's
  threads (`.config/nextest.toml`).
- New scenarios:
  - randomness-using transactions before DKG completes (deferred), then
    after (scheduled with the round's update);
  - a commit that skips a round (interval);
  - DKG failure (a committee where sui cannot complete it, with the
    timeout lowered) with `DkgFailed` cancellations;
  - congestion on randomness transactions;
  - an epoch held open by randomness deferrals;
  - a restart after rounds were certified.
- Lowering `random_beacon_dkg_timeout_round` and the stakes goes through
  the existing override hooks.

## Left out

- Peer blocking for bad partial signatures (R5.2): it protects sui's
  aggregation from load, and anchovy drops bad partials without blocking.
- The consensus observer relay (R6.4).
- Syncing certified checkpoints in general (gap 8): only a missed round's
  update is fetched. A validator left an epoch behind by a quorum that does
  not need it still cannot catch up.
