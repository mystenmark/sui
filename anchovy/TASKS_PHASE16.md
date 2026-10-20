# Phase 16 progress: compatibility with the reference implementation

Goal: anchovy and sui validators in one network, agreeing on every
checkpoint. Start with one of each (equal stake: a quorum needs both, so a
checkpoint certifies only if both built it the same), fix each fork in turn.
Most fixes are expected in the consensus handler; keep it clean and
straightforward as features accumulate.

Branched from `mlogan-phase15` (consensus integration, not yet merged).

## Harness

`crates/compat/tests/one_of_each.rs`: sui's `ConfigBuilder` makes a genesis
of three validators; sui runs two in-process (`sui_swarm::memory::Node`),
anchovy the third, with that validator's keys and the same genesis. Voting
power is capped at a third each, so the two sui validators (3,334 votes
each) hold a quorum and anchovy (3,332) can neither reject a transaction
nor hold up a certificate: its votes hide no discrepancy. A fork is
anchovy's own check panicking (a quorum certified a summary other than the
one it built), which aborts the test; a panic hook first prints the first
checkpoint the two built differently, the transactions only one lists and
the effects both list differently.

Tests: an idle network; transfers; shared objects, a shared object written
twice in one commit and through both validators at once; transactions
submitted through sui; a double spend in one block; address balance
deposits and withdrawals; epochs following one another.

## Forks found and fixed

1. Checkpoint 1 cut differently: the reference starts each epoch with no
   checkpoint cut (`last_checkpoint_flush_timestamp` 0), so an epoch's first
   commit always cuts one; anchovy counted the interval from the previous
   checkpoint.
2. The prologue's `additional_state_digest`, at the first user transaction:
   the reference folds in each scheduled user transaction's execution time
   estimate (`IndirectStateObserver`). Added `consensus::execution_time`
   (the reference's `ExecutionTimeEstimator`); the commit stage takes each
   commit's `ExecutionTimeObservation`s (checked against their block's
   author) before estimating its transactions; observations are written with
   the consensus output and restored at start-up; voting rejects durations
   the reference cannot deserialize.

3. A double spend in one block: the reference defers a transaction
   holding a lock another in the same commit tried to take
   (`defer_owned_object_double_spend`). The handler now locks before
   deduplicating, as the reference, defers contested holders a round, and
   keeps deferred transactions with the consensus output; deferred ones
   are merged first into the commit of the round they wait for.
4. Not a fork, found on the way: anchovy voted to reject funds
   withdrawals. Its input checks now take withdrawals from the sender's or
   sponsor's balance (`process_funds_withdrawals_for_signing`).
5. With sui holding a quorum, every checkpoint 3: an
   `AuthenticatorStateUpdate` anchovy lacked. JSON web keys are now agreed
   through consensus (`consensus::jwk`): votes by stake, capped per
   validator; the next commit adds keys that became active.

6. The epoch's last transaction: the reference puts more kinds before
   `ChangeEpoch` (authenticator expiry, bridge committee, stored execution
   time observations, accumulator storage cost, and creations of missing
   system objects). The closer now builds them all, from the system objects
   as the epoch started (`execution::epoch_start`, the reference's
   `EpochStartConfiguration`).
7. The epoch's last checkpoint's contents: the reference lists no
   signatures for its last transaction.
8. The next epoch's first checkpoint: chunk heights count from 1 each
   epoch in the reference, and the settlement barrier carries them.

Anchovy's aggregator now panics on a certified fork, and on a split brain
in debug builds (sui's `debug_fatal!`).

## Open

Known divergences, not yet reached by a test:
- Observations stored at the previous epoch's end are not loaded into the
  estimator at the epoch's start (only matters once validators share
  observations).
- Capabilities and protocol upgrades: the next epoch keeps the version.
- Deferral and cancellation: congestion control (budgets from the
  estimates), unpaid amplification deferral (disabled in current protocol
  versions), randomness after DKG failure (likely in a mixed network:
  anchovy takes no part in DKG).
- The signature verifier does not take keys that become active (the
  reference's `update_authenticator_state` on executing the update), so
  zkLogin transactions using them fail anchovy's checks.
- Input checks still refuse withdrawals from allowances, coin
  reservations and gas from an address balance.

Seen once and not reproduced: `execution_time_observations_outlive_a_restart`
failed under the full suite's load (took 13s); 36 parallel reruns passed.

Next tests: a crash and restart of each side, package publishing,
randomness, protocol upgrades.
