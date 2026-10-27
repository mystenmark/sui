# Sui Shared-Object Congestion Control: Behavioral Reference

This document specifies how the reference Sui validator limits per-object work in a consensus
commit: how it charges each transaction to the shared objects it touches, when it defers a
transaction to a later round, when it cancels one, and how a cancelled transaction is scheduled and
executed. It is written so that anchovy can reproduce the behavior byte for byte: every decision
here changes checkpoint contents, effects, or the commit prologue, so any divergence forks.

Requirements are numbered (R<section>.<n>) and tagged as in `reconfiguration_reference.md`:
**[E]** essential (needed for agreement), **[I]** incidental, **[E/I]** essential property,
incidental mechanism. Uncertain statements are marked **(uncertain)**. `file:line` references are
relative to the repo root at commit `5fa13b9ed3`.

| Abbrev | File |
|---|---|
| CH | `crates/sui-core/src/consensus_handler.rs` |
| SOCT | `crates/sui-core/src/authority/shared_object_congestion_tracker.rs` |
| TD | `crates/sui-core/src/authority/transaction_deferral.rs` |
| EPS | `crates/sui-core/src/authority/authority_per_epoch_store.rs` |
| CQ | `crates/sui-core/src/authority/consensus_quarantine.rs` |
| SOVM | `crates/sui-core/src/authority/shared_object_version_manager.rs` |
| ETE | `crates/sui-core/src/authority/execution_time_estimator.rs` |
| TIL | `crates/sui-core/src/transaction_input_loader.rs` |
| EP | `crates/sui-types/src/execution_params.rs` |
| TX | `crates/sui-types/src/transaction.rs` |
| BT | `crates/sui-types/src/base_types.rs` |
| PC | `crates/sui-protocol-config/src/lib.rs` |
| EXE | `sui-execution/latest/sui-adapter/src/execution_engine.rs` |

---

## 0. Where it happens

`ConsensusCommitHandler::collect_transactions_to_schedule` (CH:1384-1533) runs once per commit,
after owned-object locking and deduplication and before shared-object version assignment:

1. `merge_and_reorder_transactions` (CH:1965-2009): previously deferred transactions are loaded
   and merged with the commit's, split into regular and randomness-using lists, each reordered by
   the protocol's ordering (gas price).
2. Two congestion trackers are built (CH:1403-1406): one for regular transactions and one for
   randomness-using transactions, each seeded with its own stored per-object debts.
3. Each regular transaction, in order, then each randomness-using transaction, goes through
   `handle_deferral_and_cancellation` (CH:1759-1958). It is deferred, cancelled, or scheduled.
4. New deferrals are written to the consensus output (CH:1467-1479); each tracker's leftover
   per-object cost above the commit budget is stored as debt for the next commit (CH:1495-1526).
5. `create_pending_checkpoints` (CH:1535-1694) assigns shared versions. Cancelled transactions get
   marker versions (SOVM:443-592). The prologue lists every transaction holding a marker version
   (CH:1714-1757).

Since protocol version 102 the mode is `PerObjectCongestionControlMode::ExecutionTimeEstimate`
(PC:4369-4387); every other mode is removed and fatal (SOCT:76-90).

---

## 1. The per-commit budget and the tracker

### Parameters (current values from PC:4372-4385)

`ExecutionTimeEstimateParams` (PC:1330-1371):

| Field | Mainnet value (v102+) | Use |
|---|---|---|
| `target_utilization` | 50 (%) | budget = commit period × this / 100 |
| `allowed_txn_cost_overage_burst_limit_us` | 500_000 | extra start cost allowed above the budget |
| `randomness_scalar` | 20 (%) | scales budget and burst for the randomness tracker |
| `max_estimate_us` | 1_500_000 | cap on one transaction's estimate |
| `stake_weighted_median_threshold`, `default_none_duration_for_new_keys`, stored-observation fields | | estimator only (already in anchovy) |

Other knobs:

| Config | Value | Use |
|---|---|---|
| `max_deferral_rounds_for_congestion_control` | 10 (PC:3609) | deferral limit before cancellation |
| `max_accumulated_txn_cost_per_object_in_mysticeti_commit` | 37_000_000 (PC:3982, an old gas-budget-mode value, still read) | debt decay per skipped round |
| `max_accumulated_randomness_txn_cost_per_object_in_mysticeti_commit` | 7_400_000 (PC:3983) | the same, for randomness debts |
| `consensus_commit_rate_estimation_window_size` | 10 | commit interval observer window |
| `min_checkpoint_interval_ms` | per protocol | commit period fallback |
| `epoch_close_deadline_ms` | 120_000 from v130 (PC:4677) | how long epoch close waits on deferrals |

**R1.1 [E] The commit budget.** `Params::commit_budget` (SOCT:24-32):
`budget = estimated_commit_period_us × target_utilization / 100`, multiplied by
`randomness_scalar / 100` for the randomness tracker (saturating). The burst is
`allowed_txn_cost_overage_burst_limit_us`, also scaled by `randomness_scalar / 100` for
randomness (SOCT:34-40).

**R1.2 [E] The estimated commit period.** `AdditionalConsensusState::observe_commit` (CH:235-257)
first pushes the current commit's timestamp into `CommitIntervalObserver` (ring of
`consensus_commit_rate_estimation_window_size` timestamps, CH:3591-3622). It then takes
`commit_interval_estimate()`: with more than one timestamp, `(last - first).div_ceil(len)` ms
(note: divided by the number of timestamps, not intervals); otherwise
`Duration::from_millis(min_checkpoint_interval_ms)`. This observer is the one whose digest the
prologue records, and anchovy already keeps it (`commit.rs` `CommitIntervalObserver`, which has
`observe` and `digest` but no `commit_interval_estimate`). The estimate is in whole
milliseconds, converted to microseconds (`as_micros`, SOCT:26).

**R1.3 [E] A transaction's cost.** `get_tx_cost` (SOCT:100-122): the estimator's
`get_estimate(transaction_data)` in microseconds, saturating to `u64::MAX` (ETE:846-865: the sum
over commands of the key's stake-weighted median or default duration, times the command's length,
capped at `max_estimate_us`). Anchovy's `ExecutionTimeEstimator::estimate_us` is this. **The cost
is also observed into the `IndirectStateObserver`** (SOCT:119), whose digest goes into the
prologue's additional state digest. See R1.8 for exactly which transactions are observed.

**R1.4 [E] Start cost.** `compute_tx_start_at_cost` (SOCT:92-98): the maximum, over **every**
shared input object of the transaction (mutable, read-only, or non-exclusive write), of that
object's accumulated cost in the tracker (0 if absent). Shared inputs are `cert.shared_input_objects()`
(TX), in input order. A transaction with no shared inputs is never deferred (SOCT:133-135).

**R1.5 [E] Deferral test.** `should_defer_due_to_object_congestion` (SOCT:124-163):
defer iff `start_cost > budget + burst` (strictly greater, saturating add). There is no
special case for the first transaction on an object: a transaction starting at cost 0 is always
admitted, however large its own estimate. The overage is bounded by the burst only on the start
cost; the end cost may exceed `budget + burst` by up to one transaction's estimate (capped at
1.5 s). The congested objects are every shared input whose current cost equals `start_cost`
(SOCT:144-149), in input order; at least one exists.

**R1.6 [E] Charging.** `bump_object_execution_cost` (SOCT:165-192), called only for scheduled
transactions (CH:1953-1956): `end = start_cost + tx_cost` (saturating); every shared input
accessed **exclusively** (`SharedObjectMutability::Mutable`; `is_accessed_exclusively` is
`is_exclusive`, TX:1927, TX:4939-4945) is set to `end`. Read-only and `NonExclusiveWrite` inputs
are not charged, but they still raise the start cost (R1.4). Deferred and cancelled transactions
charge nothing.

**R1.7 [E] Debt carried between commits.**
- `finish_commit` (SOCT:194-216): each object's final cost minus the commit budget, kept if
  positive, becomes `accumulated_debts`.
- They are stored in the commit's output with the commit's round (`set_congestion_control_object_debts`,
  CQ:256-265; written as `CongestionPerObjectDebt::V1(consensus_round, debt)`, CQ:381-400 and the
  in-memory map CQ:688-712). Regular and randomness debts are kept in separate tables.
- **Only positive debts are written. An object whose cost ends under the budget leaves its earlier
  debt record in place**; nothing deletes it.
- At the next commit, `load_initial_object_debts` (CQ:919-984) reads the debts of every shared input
  object of the commit's transactions (deduplicated, sorted) and decays each:
  `debt - per_commit_budget × (current_round - debt_round - 1)` (saturating), where
  `per_commit_budget` is **`max_accumulated_txn_cost_per_object_in_mysticeti_commit`** (37_000_000),
  or the randomness variant for the randomness tracker. It is not the dynamic commit budget.
  `assert!(current_round > round)`.
- In current config a debt survives undiminished only into the commit at exactly the next leader
  round; one skipped round removes up to 37 s, which zeroes any realistic debt. The formula must
  still be reproduced exactly, including stale records (essential for agreement whenever a debt is
  larger than the decay, e.g. in tests that override the budget).

**R1.8 [E] Order and observation.** Within `handle_deferral_and_cancellation` (CH:1759-1958), for
each transaction in order:
1. Unpaid amplification (`defer_unpaid_amplification`, off in current versions): may defer and return.
2. Owned-object double-spend contest (`defer_owned_object_double_spend`): defer and return, if within
   the limit. **If the limit is exceeded it falls through** to the congestion steps.
3. `get_tx_cost`: the estimate is observed into the `IndirectStateObserver` here, **before** the
   deferral decision. Every transaction reaching this step is observed, including those then
   deferred for randomness or congestion and those cancelled for congestion.
4. `EPS::should_defer` (EPS:2368-2409): randomness first (R2.4), then the congestion test (R1.5).
5. If deferred for congestion: within the limit → defer; beyond it → cancel (`CongestionOnObjects`)
   and **schedule it in place** (R3).
6. Otherwise charge (R1.6) and schedule.

Regular transactions all go before randomness-using ones (CH:1424-1465), so observations go in
that order too. Randomness-using transactions cancelled because DKG failed are cancelled before
this function and are **not** observed (CH:1438-1451).

**R1.9 [I] Side effects with no consensus impact.** The union of debt object ids is sent to the
local `ExecutionTimeObserver` (`tx_object_debts`, CH:1506-1519), which uses it to decide which of
its own observations to share. It only matters if anchovy ever submits observations. Metrics and the
congestion logger are local.

---

## 2. Deferral

**R2.1 [E] Keys.** `DeferralKey` (TD:6-17) is `Randomness { deferred_from_round }` (variant 0) or
`ConsensusRound { future_round, deferred_from_round }` (variant 1), ordered derived (variant, then
fields). Rounds are **leader rounds** (`commit_info.round`), not commit indices. Anchovy's
`deferral.rs` `DeferralKey` matches.

**R2.2 [E] A congestion deferral's key.** Future round `commit_round + 1`; `deferred_from_round` is
the previous key's, if the transaction was loaded as deferred, else `commit_round` (SOCT:153-161).
The double-spend and amplification paths use the same rule (CH:1804-1812, CH:1855-1863).

**R2.3 [E] The limit.** `transaction_deferral_within_limit` (TD:83-97): for `ConsensusRound`,
`future_round - deferred_from_round <= max_deferral_rounds_for_congestion_control`; `Randomness`
keys are always within. Since leader rounds can skip, the limit counts rounds, not commits.

**R2.4 [E] Randomness deferral.** `should_defer` defers a randomness-using transaction with a
`Randomness` key (keeping `deferred_from_round`) if randomness is enabled, DKG has not failed and
this commit generates no randomness round (EPS:2377-2392). This happens before the congestion
test, so randomness-using transactions are congestion-checked only in commits that generate
randomness. If DKG failed, they are cancelled with `DkgFailed` (CH:1438-1451).

**R2.5 [E] Re-loading.** `load_deferred_transactions` (CH:2011-2067):
- **What:** all `ConsensusRound` keys up to and including the commit's round, in key order and in
  stored order within a key; plus all `Randomness` keys, only if this commit generates randomness
  or DKG failed. Each loaded transaction's previous key goes into `previously_deferred_tx_digests`.
- **Merge:** loaded transactions come first. Loaded randomness-using transactions deferred by
  congestion move to the randomness list. The commit's transactions follow, and each list is then
  reordered (stable, by gas price) (CH:1965-2009).
- **Anchovy:** already merges this way for its double-spend deferrals (`commit.rs` step 6, `take_deferred(DeferralKey::due_by(round))`).

**R2.6 [E] Storage.** New deferrals are written per key into the consensus output and the
epoch's `deferred_transactions` cache (CH:1467-1479); loading deletes the loaded keys
(`delete_loaded_deferred_transactions`). Anchovy's epoch store does the same (`output.defer`,
`take_deferred`).

**R2.7 [E] Epoch close waits on deferrals.** `advance_end_of_epoch_state_machine`
(CH:2366-2413): once the epoch's time is up, the final round is the first commit with no
deferred transactions, from this commit or earlier ones. The keys a commit loaded are removed from the
deferred-transactions cache only after this check (`record_deferral_deletion`, CH:1252, after
`handle_close_epoch`, CH:1189), so a commit that loads any deferred transaction is never the final
one, even if it schedules them all. Until then the epoch stays open, with
user certificates closed. Past `epoch_close_deadline_ms` (120 s, from v130) it closes regardless,
and `abandon_deferred_transactions` (CH:2420-2460) drops the remainder. Anchovy's stand-in ends
the epoch at the first commit past its end time (TASKS_PHASE16 "Not here yet"). That is already
wrong for double-spend deferrals, and congestion makes it reachable easily.

---

## 3. Cancellation

**R3.1 [E] Scheduled in place.** A cancelled transaction stays in the schedule at its position
(CH:1942-1947), so it appears in the checkpoint like any other transaction, with effects.
Cancellation reasons: `CancelConsensusCertificateReason::CongestionOnObjects(congested_ids)` or
`DkgFailed`.

**R3.2 [E] Marker versions.** `assign_versions_for_certificate` (SOVM:443-592), for a cancelled
transaction:
- **The markers:** every shared input, in input order, is assigned a marker instead of a version
  (SOVM:498-528).
  - For a congestion cancellation: `SequenceNumber::CONGESTED` (`MAX + 2`) for each congested
    object, `CANCELLED_READ` (`MAX + 1`) for the other shared inputs.
  - For `DkgFailed`: `RANDOMNESS_UNAVAILABLE` (`MAX + 3`) for the randomness state,
    `CANCELLED_READ` for the others (BT:1504-1507).
- **Versions do not advance.** None of the transaction's accesses advances a shared object's next
  version (SOVM:560-581): the next transaction on each object reads the same version it would have
  read had the cancelled one not existed.
- **Lamport version:** it is computed from the non-shared inputs (owned objects, gas, receiving)
  only (SOVM:552-558).

**R3.3 [E] The prologue lists them.** `add_consensus_commit_prologue_transaction` (CH:1714-1757)
walks the assigned versions in schedule order. Regular chunks come first, then randomness chunks
when written (CH:1595-1609). For every transaction key that is a digest with any cancelled marker,
it pushes `(digest, full shared-version list)`: all of the transaction's shared inputs, markers
included, as `((id, initial_shared_version), SequenceNumber)`. These go into
`ConsensusCommitPrologueV4`'s `ConsensusDeterminedVersionAssignments::CancelledTransactionsV2`
(CH:381-401). Anchovy always writes an empty list (`commit.rs`
`CancelledTransactionsV2(Vec::new())`).

**R3.4 [E] Execution.**
- **Inputs:** the input loader gives each marker-versioned shared input as
  `ObjectReadResultKind::CancelledTransactionSharedObject(marker)`, without reading the object
  (TIL:183-197).
- **Early error:** `get_early_execution_error` (EP:82-123) collects, in order: certificate denied,
  consensus stream ended, cancellation, insufficient funds. The cancellation is
  `ExecutionCancelledDueToSharedObjectCongestion { congested_objects }` for `CONGESTED`, or
  `ExecutionCancelledDueToRandomnessUnavailable` for `RANDOMNESS_UNAVAILABLE`, where the congested
  objects are the inputs holding the reason marker, in input order (`get_cancelled_objects`,
  TX:5272-5299; it asserts a single reason).
- **Status and gas:** the engine fails the transaction with the **first** early error
  (EXE:799-800, EXE:1259-1260), without running it. Gas is charged as for any early failure.
- **Effects:** cancelled shared inputs appear in the effects' unchanged consensus objects as
  `UnchangedConsensusKind::Cancelled(marker)`.
- **Anchovy:** its executor already ports all of this (`executor/src/inputs.rs`
  `InputState::Cancelled`, `get_cancelled_objects`; `executor/src/execution_params.rs`;
  `executor/src/effects.rs` `SharedInput::Cancelled`). What is missing is a way to pass a cancelled
  input in: `execution::Input` has no `Cancelled` variant (`execution/src/lib.rs`), and neither does
  native `load` (`native.rs`) nor the reference path's `load_at`.

---

## 4. Interaction with the execution time estimator

**R4.1 [E]** The tracker's cost is exactly the estimator's estimate (R1.3). Observations a commit
carries are applied before its transactions are estimated (anchovy step 4 already). No other state
links them: the per-object debt is the tracker's own (R1.7).

**R4.2 [E]** The `IndirectStateObserver` sees the estimate of every transaction reaching
`get_tx_cost`, in processing order (R1.8). Anchovy pushes an estimate only for transactions it
admits (`commit.rs` step 7, `estimates_us`). That is correct today only because its sole
deferral path, the double spend, returns before the estimate, as the reference's does.

---

## 5. Anchovy today and the gaps

**What exists**
- **Commit handler** (`validator/src/consensus/commit.rs` `CommitHandler::handle`):
  - step 6 loads due deferrals and merges them first;
  - `Sequencer::order` puts randomness-using transactions last and sorts by gas price;
  - step 7 defers contested double spends with `DeferralKey::next_round`, and otherwise estimates
    and admits;
  - step 8 schedules with ordinals.
- **Deferral** (`deferral.rs`): keys and encoding match.
- **Estimator** (`execution_time.rs`): observations, the median and the cap match.
- **Commit interval observer**: timestamps and digest match; there is no interval estimate.
- **Executor**: handles cancelled inputs, statuses and effects.

**Gaps that fork once a commit is congested**
1. No congestion tracker: no budget (R1.1, R1.2), start cost, deferral test, charging, or debt
   (R1.4-R1.7). A sui validator defers or cancels a transaction that anchovy schedules, so the
   schedules differ.
2. No debt storage: the epoch store needs per-object `(round, debt)` for regular and randomness
   trackers, written with the commit's output, never deleted, decayed with the fixed config value
   on load (R1.7).
3. Observation order: the `IndirectStateObserver` must include congestion-deferred and cancelled
   transactions, in processing order: regular first, then randomness (R1.8, R4.2).
4. The double spend past its limit must fall through to the congestion test, not straight to
   admission (R1.8 step 2).
5. Cancellation scheduling:
   - **Representation:** a cancelled transaction is scheduled with marker versions. Anchovy's
     `sequence.rs` gives every shared access a `SharedOrdinal` and counts it among the stream's
     writers. A cancelled access must take no ordinal, must not count as a writer (R3.2), and must
     carry its marker (`CONGESTED` or `CANCELLED_READ`).
   - **Downstream:** the resolver must give such an access no input key; exec-pool must pass the
     new `Input::Cancelled(marker)` and record no generation for it; `link` must skip it.
   - **Lamport version:** it then comes from owned inputs only, as the executor already computes.
6. The prologue must list cancelled transactions with their full shared-version lists, in schedule
   order (R3.3), instead of an empty `CancelledTransactionsV2`.
7. Epoch close must wait for deferred transactions to drain, up to `epoch_close_deadline_ms`, then
   abandon them (R2.7).
8. Randomness: randomness-deferral (R2.4), the randomness tracker and debts, and `DkgFailed`
   cancellation all depend on knowing whether the commit generates a randomness round or DKG failed.
   Anchovy takes no part in DKG and cannot know this today. This is out of scope for congestion
   proper. Randomness-using transactions are the only ones whose schedule can still differ, and only
   in a network where randomness is enabled.

**Forcing congestion in a compat test.** `sui-protocol-config` keeps a process-global override
(`apply_overrides_for_testing`, PC:4956), which anchovy, now on the same crate, sees too. Apply it
before any config is read; each nextest test is its own process. Useful settings:
- `per_object_congestion_control_mode = ExecutionTimeEstimate { target_utilization: 0,
  allowed_txn_cost_overage_burst_limit_us: 0, .. }` (setter
  `set_per_object_congestion_control_mode_for_testing`, PC:5002). The budget is then 0. Of several
  transactions writing one shared object in a commit, the first is admitted and the rest are
  deferred: the first starts at cost 0, then raises the object to its estimate, ≥ 1 ms for a Move
  call by default.
- `max_deferral_rounds_for_congestion_control = 0`
  (`set_max_deferral_rounds_for_congestion_control_for_testing`): the first deferral already exceeds
  the limit (`future - from = 1 > 0`), so the transaction is cancelled at once. A small positive
  value exercises re-deferral, then cancellation.
- With budget 0 the debt is the object's full cost and carries into the next round's commit
  (R1.7), deferring the next commit's first transaction on that object too.
- To control the decay, override `max_accumulated_txn_cost_per_object_in_mysticeti_commit` (and the
  randomness variant), e.g. to 0, so debts never decay.
- A smaller `min_checkpoint_interval_ms`, or nonzero `target_utilization`, varies the budget with the
  observed commit interval.
- Submitting several transactions on one shared object in one request puts them in one block, and
  so in one commit.
- The scenario to check is that sui and anchovy agree on: the deferred and cancelled sets, the
  prologue's cancelled list, the cancelled transactions' effects
  (`ExecutionCancelledDueToSharedObjectCongestion` with the congested ids, `Cancelled` unchanged
  consensus objects), and the next transaction's versions.
