# Protocol upgrades and congestion control: design

References: `protocol_upgrade_reference.md` (R-numbers prefixed PU) and
`congestion_control_reference.md` (prefixed CC).

## Goals

- Anchovy's stake counts toward a protocol upgrade, and anchovy's
  change-epoch transaction names the version and system packages sui's
  does (PU gaps 1–5).
- Anchovy defers and cancels exactly the transactions sui does in a
  congested commit, schedules a cancelled one as sui does, and lists it in
  the prologue (CC gaps 1–6).
- Both need the end of the epoch as sui decides it (CC R2.7, PU R1.6): the
  epoch stays open while deferred transactions remain.

Out of scope: randomness. Its deferral, its tracker and `DkgFailed`
cancellation need to know whether a commit generates randomness, which
anchovy cannot (CC gap 8). Randomness-using transactions keep today's
handling, so the network must not enable randomness.

## 1. The end of the epoch

Today `CommitStage::decide` closes the epoch at the first commit at or
past `epoch_start_timestamp_ms + epoch_duration_ms`. Instead, as sui's
`handle_close_epoch` and `advance_end_of_epoch_state_machine`:

- **Certificates close** at the first commit past the end time. The commit
  itself still processes everything. From the next commit on, the handler
  drops new user transactions (status `Dropped`), capability notifications,
  JWK votes and execution time observations. Checkpoint signatures are still
  routed. Deferred transactions are still loaded and scheduled.
- **The final commit** is the first commit, from the closing one on, after
  which no deferred transaction remains, counting this commit's new
  deferrals and earlier ones. It gets the change-epoch reservation as the
  closing commit does today.
- **The deadline:** a commit at or past end time + `epoch_close_deadline_ms`
  is final regardless. Its new deferrals are dropped and every remaining
  deferral key is deleted (`abandon_deferred_transactions`).
- **State:** the handler keeps a `certs_closed` flag in its commit record,
  so a replay after a restart decides alike. Commits after the final one are
  ignored, as today.

Anchovy's double-spend deferrals already need this; congestion makes it
common.

## 2. Protocol upgrades

### Sending capabilities (PU R1.1–R1.4)

At each epoch's start (node start and after reconfiguration), a validator in
the committee submits a `CapabilityNotificationV2` through its consensus
`TransactionClient`, like a checkpoint signature:

- the generation is the wall clock in milliseconds;
- the supported versions are the binary's range, truncated below the
  current version, each with its config digest, with sui's accumulator
  caveat (PU R1.4);
- the available system packages come from `compare_system_package` over
  anchovy's store.

The execution crate already links `sui-protocol-config` and
`sui-framework`, so both digests and packages come from sui's own code
(`supported_versions_with_digests`, `available_system_packages` in
`execution`). Anchovy's own `protocol-config` copy is never hashed, so it
cannot drift from sui's digest (decision 1).

### Recording them (PU R1.5, R1.6)

The commit decoder decodes `CapabilityNotificationV2` and drops one whose
authority is not the block's author. The commit handler keeps, per
authority, the one with the highest generation, until certificates close.
The set goes in the consensus output with the commit, as JWK votes do, so a
restart's replay rebuilds it.

### Choosing and writing the version (PU R2.1–R2.4)

The final commit's `EpochClose` carries the capability set, as it carries
the observations. `EpochCloser` runs a port of
`choose_protocol_version_and_system_packages_v2` with the epoch's committee
and `buffer_stake_for_protocol_upgrade_bps`. It then writes the chosen
version, and `get_system_package_bytes`'s packages, into the `ChangeEpoch`.
The next epoch already follows the system state's version.

### Unsupported versions (PU R3.3)

At an epoch's start, anchovy exits with an error if the version is outside
its binary's range, instead of panicking in `get_for_version`.

## 3. Congestion control

### The tracker (CC R1.1–R1.6)

A `CongestionTracker` in `validator::consensus`, one per commit:

- **Budget:** commit period × `target_utilization` / 100. The period is the
  `CommitIntervalObserver`'s estimate, including this commit (R1.2). The
  observer moves from `CommitStage` into `CommitHandler`, observed at the
  start of `handle`. Replayed prior commits only observe.
- **Start cost:** the highest cost among all the transaction's shared
  inputs. A transaction is deferred if its start cost exceeds budget +
  burst. Congested objects are the inputs at that cost.
- **Charge:** a scheduled transaction sets its exclusive inputs to start +
  estimate. Read-only and non-exclusive inputs are not charged.
- **Order (R1.8):** double-spend contest first, falling through past its
  limit. Then the estimate, observed into the indirect state digest for every
  transaction that reaches this step. Then the congestion test.

### Debts (CC R1.7)

At the end of a commit, each object's cost above the budget is stored with
the commit's round, in the consensus output. Only positive debts are written
and none is deleted, as in sui. A commit loads the debts of its transactions'
shared inputs and decays each by
`max_accumulated_txn_cost_per_object_in_mysticeti_commit` per skipped round.
Debts are epoch state in the epoch store (`anchovy-store-placement`).

### Deferral and cancellation (CC R2, R3.1–R3.3)

- **Within the limit:** deferral works as today's double-spend deferral,
  with the same keys and storage.
- **Past the limit:** the transaction is cancelled and stays in the
  schedule at its place.
- **Accesses:** a cancelled transaction's accesses take no ordinal and do
  not count as writers. Each carries its marker instead (`CONGESTED` for
  the congested inputs, `CANCELLED_READ` for the others).
  `SharedAccess.ordinal` becomes an enum, `Ordinal(SharedOrdinal) |
  Cancelled(marker)`.
- **Downstream:**
  - `link` and the resolver skip cancelled accesses;
  - exec-pool passes a new `execution::Input::Cancelled(marker)` and
    records no generation for them;
  - the executor already handles cancelled inputs, their status and
    effects (R3.4).
- **Prologue:** cancelled transactions are listed in schedule order with
  their shared inputs and markers. A cancelled transaction has only
  markers, so no real version is needed at commit time.

## Testing

`crates/compat`, with sui validators holding a quorum alone:

- **Upgrade:** genesis at `MAX - 1`, short epochs. Anchovy's capabilities
  are needed for the 8,334 threshold, so the network reaches `MAX` at the
  first epoch end only if anchovy's count. Anchovy must then write `MAX`.
- **Split vote:** one sui validator supports only `MAX - 1`, so there is no
  upgrade, and anchovy agrees.
- **Congestion:** the same override applied to both protocol-config crates
  before any config is read. With target utilization 0 and burst 0, several
  transactions on one shared object in one block defer all but the first.
  With `max_deferral_rounds_for_congestion_control = 0` they are cancelled.
  With a small limit they re-defer, then cancel. With decay 0, debts carry.
- **Epoch close with deferrals:** deferrals pending at the end time keep
  the epoch open. A short deadline abandons them.

Unit tests: the choose rule against sui's own cases; the tracker against
sui's `shared_object_congestion_tracker` tests; the epoch-close state
machine against `consensus_handler`'s deadline tests.

## Order of work

1. The end of the epoch (§1).
2. Protocol upgrades (§2), with the upgrade and split-vote scenarios.
3. Congestion (§3): the tracker and debts, then cancellation, each with
   its scenario.

## Decisions

1. Digests and packages come from sui's `sui-protocol-config` and
   `sui-framework`, through the execution crate, rather than from anchovy's
   copy. The copy can't make the same digest by accident.
2. Randomness is out of scope (above).
3. Framework upgrades: the package rule is ported, but compat tests only
   version upgrades. A framework upgrade needs other framework bytes, which
   sui tests only in simulation.
4. Capabilities and debts go in the quarantined consensus output, not
   straight to the table as sui writes capabilities. A replay rebuilds them
   either way, and anchovy keeps one writer for consensus output.
