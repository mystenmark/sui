# Sui Epoch Change and Reconfiguration: Behavioral Reference

This document specifies how the reference Sui validator ends an epoch and reconfigures itself into
the next one. It is written so that a from-scratch reimplementation (anchovy) can reproduce the
required behavior and tell which parts are artifacts of the reference's structure.

Each requirement is numbered (R<section>.<n>). It states the behavior, then usually the reason,
with `file:line` references. Line numbers are relative to the repo root at commit `5c1b6c825e`.
Requirements are tagged:

- **[E] Essential.** Any correct implementation must do this. It is needed for agreement with
  other validators (consensus or forks), for safety, or for liveness.
- **[I] Incidental.** This is how the reference happens to do it. A reimplementation may choose
  differently if it keeps the essential properties.
- **[E/I]** The property is essential, but the mechanism is incidental.

Uncertain statements are marked **(uncertain)**.

Abbreviations:

| Abbrev | File |
|---|---|
| NODE | `crates/sui-node/src/lib.rs` |
| AUTH | `crates/sui-core/src/authority.rs` |
| EPS | `crates/sui-core/src/authority/authority_per_epoch_store.rs` |
| ESC | `crates/sui-core/src/authority/epoch_start_configuration.rs` |
| AS / AST | `crates/sui-core/src/authority/authority_store.rs` / `authority_store_tables.rs` |
| RECONF | `crates/sui-core/src/epoch/reconfiguration.rs` |
| CH | `crates/sui-core/src/consensus_handler.rs` |
| CA | `crates/sui-core/src/consensus_adapter.rs` |
| CM | `crates/sui-core/src/consensus_manager/mod.rs` |
| CP | `crates/sui-core/src/checkpoints/mod.rs` |
| CE / CEU | `crates/sui-core/src/checkpoints/checkpoint_executor/mod.rs` / `utils.rs` |
| GSH | `crates/sui-core/src/global_state_hasher.rs` |
| EC / WBC | `crates/sui-core/src/execution_cache.rs` / `execution_cache/writeback_cache.rs` |
| ESI | `crates/sui-core/src/execution_scheduler/execution_scheduler_impl.rs` |
| ED | `crates/sui-core/src/execution_driver.rs` |
| SRV | `crates/sui-core/src/authority_server.rs` |
| STOR | `crates/sui-core/src/storage.rs` |
| SS | `crates/sui-network/src/state_sync/mod.rs` |
| MCK | `crates/sui-types/src/messages_checkpoint.rs` |
| EXE | `sui-execution/latest/sui-adapter/src/execution_engine.rs` |
| NR | `crates/sui-types/src/node_role.rs` |

Terms:

- **Epoch store**: `AuthorityPerEpochStore`, which holds all per-epoch state, including a per-epoch
  RocksDB at `<db>/epoch_<N>` (EPS:785-787).
- **Final checkpoint** or **last checkpoint**: the unique checkpoint of epoch N whose summary
  carries `end_of_epoch_data`. `is_last_checkpoint_of_epoch()` is defined as
  `end_of_epoch_data.is_some()` (MCK:453-455).
- **Change-epoch tx**: the `EndOfEpochTransaction` whose final element is `ChangeEpoch`. Older
  protocol versions used a bare `ChangeEpoch` transaction. Both are "end of epoch txs"
  (`is_end_of_epoch_tx()`).

---

## 0. End-to-end timeline (overview)

The happy path for a validator that stays in the committee:

1. **Close.** A consensus commit arrives whose timestamp is at or past
   `epoch_start_timestamp_ms + epoch_duration_ms`. The consensus handler moves the reconfig state
   to `RejectAllCerts`. Once no deferred transactions remain, it moves to `RejectAllTx` and marks
   the last `PendingCheckpoint` of that commit `last_of_epoch = true` (CH:1309-1345,
   CH:2364-2414, CH:1683-1685). Later commits are ignored, except for checkpoint signatures.
2. **Build.** The checkpoint builder builds the final checkpoint. It waits until every earlier
   checkpoint of the epoch has been executed locally, constructs the change-epoch tx and executes
   it **without committing**, appends its effects, computes the epoch's root state hash, and fills
   in `EndOfEpochData` (CP:2057-2122, AUTH:6033-6249). It then signs the checkpoint and submits
   the signature to consensus.
3. **Certify.** The aggregator collects a quorum of signatures, inserts the certified checkpoint
   (which also records `epoch_last_checkpoint_map[N]`), and hands it to state sync, which bumps
   `highest_synced` (CP:823-848, SS:899-924).
4. **Execute.** The checkpoint executor reaches the final checkpoint. It waits for checkpoint
   `seq-1` to be executed, executes the change-epoch tx for real through the scheduler using the
   certified effects digest, commits all outputs, persists the epoch's last checkpoint and root
   state hash, bumps `highest_executed`, and returns `StopReason::EpochComplete` (CE:357-529,
   CE:996-1065).
5. **Reconfigure.** `SuiNode::monitor_reconfiguration` does the following (NODE:1924-2301):
   - reads the new `SuiSystemState` from the object store and derives the
     `EpochStartSystemState` and committee for N+1;
   - calls `AuthorityState::reconfigure`, which inserts the committee, takes the execution write
     lock, terminates epoch tasks, clears the dirty cache, persists the new
     `EpochStartConfiguration`, opens the epoch N+1 store and swaps it in;
   - shuts down consensus for N, prunes old consensus DBs, and starts the per-epoch components
     for N+1 (checkpoint service, consensus, randomness, JWK updater, and so on);
   - releases the old epoch store's DB handles and loops, creating a new `CheckpointExecutor` for
     N+1.

The rest of this document specifies each step.

---

## 1. Deciding that the epoch ends (brief)

The rewrite will not implement end-of-epoch determination yet. It assumes the consensus handler
at some point emits the final `PendingCheckpoint`. This section only records the parts that
determine what that final pending checkpoint looks like.

**R1.1 [E] The epoch closes on a consensus-agreed timestamp.** The trigger is
`commit_info.timestamp >= next_reconfiguration_timestamp_ms()`, where the right-hand side is
`epoch_start_timestamp_ms + epoch_duration_ms` from the `EpochStartSystemState` (CH:1320-1321,
EPS:1224-1230). The commit timestamp is agreed by consensus, so every validator closes at the same
commit. A quorum of `EndOfPublish` messages is a second trigger (CH:1332-1334, CH:2326-2361). It
survives only for manual or admin closes (`close_epoch`, CA:976-1003, NODE:1107-1117). It is
deprecated, so the rewrite can ignore it.

**R1.2 [E] The reconfig state machine is a pure function of committed consensus output.** The
states are `AcceptAllCerts`, then `RejectUserCerts` (manual close only), then `RejectAllCerts`,
then `RejectAllTx` (RECONF:10-26). Each commit is filtered against the reconfig state as it stood
at the start of the commit (`initial_reconfig_state`, CH:953, CH:2690-2741):

- Under `RejectAllCerts`, newly sequenced user transactions, capability notifications,
  execution-time observations, JWKs and deny-config updates are dropped. User transactions get a
  terminal `Dropped` status. Checkpoint signatures and randomness DKG messages are still
  processed (CH:2690-2733).
- Under `RejectAllTx`, DKG messages are also dropped (CH:2735-2740).

The new state is written into the commit's output batch (`store_reconfig_state`, CH:2410), so it
is persisted atomically with the commit.

**R1.3 [E] The final round waits for deferred transactions, with a deadline.** Once triggered,
every later commit enters `advance_end_of_epoch_state_machine` (CH:2366-2414):

- It moves to `RejectAllCerts`.
- It declares the final round only when no deferred transactions remain, counting both this
  commit and earlier ones (`should_close = !has_deferred_txns || deadline_reached`).
- `deadline_reached` means the commit timestamp is at or past
  `next_reconfiguration_timestamp_ms + epoch_close_deadline_ms`. That parameter is 120 s from
  protocol version 130 (CH:1322-1331, `crates/sui-protocol-config/src/lib.rs:4677`).
- Past the deadline, any remaining deferred transactions are abandoned deterministically
  (CH:2400-2402), and the abandonment is reported only after the commit's output has been pushed
  (CH:1280-1294).
- `final_round` is true only on the transition into `RejectAllTx`. A commit that starts in
  `RejectAllTx` is never final (CH:2393).

**R1.4 [E] After the final round, no further checkpoints are produced.** `make_checkpoint` is
`should_accept_tx || final_round` (CH:1191). Commits after the final one return early without
scheduling anything and without pushing consensus output (CH:1192-1195). Their checkpoint
signatures are still delivered, because `process_checkpoint_signature_messages` runs before the
early return (CH:1156). Without that, the final checkpoint could not be certified.

**R1.5 [E] Randomness stops at the final round.** A new randomness round is reserved only while
`initial_reconfig_state.should_accept_tx()` holds (CH:989-996). If DKG failed, deferred randomness
transactions are cancelled with `DkgFailed` (CH:1440-1447). They then flow into a "random"
checkpoint (CH:1551-1552), which matters for R1.6.

**R1.6 [E] The final commit produces exactly one `last_of_epoch` pending checkpoint, and it comes
last.** In `create_pending_checkpoints` (CH:1535-1702):

- On the final round the checkpoint queue is force-flushed (`flush(ts, final_round)`, CH:1666).
- Randomness chunks, if any, are pushed and force-flushed after it (CH:1669-1681).
- Only `pending_checkpoints.last_mut()` gets `last_of_epoch = true` (CH:1683-1685).

Bug 752fc35fe0 produced two end-of-epoch checkpoints when DKG had failed (§5). The final pending
checkpoint's `timestamp_ms` is the final commit's timestamp (CH:684-709), and it becomes the next
epoch's start timestamp (R2.4).

**R1.7 [I] Checkpoint sequence numbers are pre-assigned by the consensus handler.** In this fork,
`PendingCheckpointInfo.checkpoint_seq` is assigned by `CheckpointQueue` (CP:139-148, CH:684-709). At epoch start the counter resumes from
`epoch_store.previous_epoch_last_checkpoint() + 1` (CH:808-811, CH:826). The **essential**
property is that the first checkpoint of epoch N+1 is `last_checkpoint(N) + 1`.

**R1.8 [I] What the consensus adapter and gRPC service reject during close.**

- `submit_and_get_positions` returns `ValidatorHaltedAtEpochEnd` unless the reconfig state
  accepts user certs (CA:290-293).
- Submissions in flight are wrapped in `within_alive_epoch`, so they are cancelled at
  reconfiguration (CA:427-436, CA:1060).
- `handle_submit_transaction` retries once after a `ValidatorHaltedAtEpochEnd`. It waits up to
  15 s for `wait_for_epoch(start_epoch + 1)` (SRV:648-692, AUTH:3917-3926).

The essential part is that a client is told to retry and the transaction is not silently lost.
The retry inside the server is a convenience.

---

## 2. The last checkpoint

### 2.1 Builder control flow at the epoch end

**R2.1 [E] The final checkpoint is built from the final pending checkpoint like any other, and
then augmented.** `create_checkpoint` (CP:1908-2193) does the following when
`details.last_of_epoch` is set (CP:2037, CP:2057):

1. Resolves and sorts the effects of the roots, as for any checkpoint.
2. Computes the rolling epoch gas summary over the *user* effects (CP:2054-2055, R2.3).
3. Calls `augment_epoch_last_checkpoint` (CP:2220-2247), which builds and executes the
   change-epoch tx and **appends** its effects as the last transaction, with an empty signature
   list (CP:2244-2245).
4. Accumulates the state hash of the full effects list, including the change-epoch tx. It waits
   for the running root of `seq-1`, folds this checkpoint into it, and finalizes the epoch's root
   (CP:2077-2100).
5. Builds `EndOfEpochData` from the post-advance system state (CP:2112-2118).

For a non-final checkpoint, the effects are sent to the asynchronous hasher task instead
(CP:2120-2124).

**R2.2 [E] The change-epoch tx is the last transaction of the last checkpoint, and it appears
nowhere else.**

- The builder treats an end-of-epoch tx among the resolved effects as fatal (CP:1965-1971).
- The executor asserts the last transaction is an end-of-epoch tx (CE:1004-1011).
- When scheduling the rest of the final checkpoint, the executor skips it (CE:951-952) and
  executes it separately afterwards (CE:657-661).

**R2.3 [E] Gas cost summary.** `epoch_rolling_gas_cost_summary` is the sum of
`GasCostSummary::new_from_txn_effects` over all checkpoints of the current epoch so far, and it
resets when the previous checkpoint belongs to a different epoch (CP:2196-2218). For the final
checkpoint it covers the effects *before* the change-epoch tx is appended. That value is passed
to `create_and_execute_advance_epoch_tx` (CP:2059-2061) and also stored in the final summary
(CP:2165-2177).

- `computation_charge = computation_cost`
- `storage_charge = storage_cost`
- `storage_rebate = storage_rebate`
- `non_refundable_storage_fee = non_refundable_storage_fee`

These are the epoch totals (AUTH:6141-6150).

**R2.4 [E] The next epoch's start timestamp is the final checkpoint's timestamp.**
`augment_epoch_last_checkpoint` is called with `epoch_start_timestamp_ms = timestamp_ms`, the
final pending checkpoint's timestamp (CP:2039-2040, CP:2059-2062). It goes into
`ChangeEpoch.epoch_start_timestamp_ms` and from there into the system state. The next epoch's
consensus is started with that timestamp (CM:388-390). Checkpoint timestamps must not decrease
(CP:2041-2050).

**R2.5 [E] The builder's last checkpoint waits for every earlier checkpoint of the epoch to be
executed locally.** `wait_for_previous_running_root(seq)` returns immediately only when `seq-1`
is the previous epoch's last checkpoint. Otherwise it waits on `notify_read_running_root(seq-1)`
(GSH:479-503). The running root of a non-final checkpoint is produced only by the checkpoint
executor (CE:502-504). So a validator cannot locally build the final checkpoint until it has
*executed* everything before it.

- Why: the root state hash is a running union over every checkpoint of the epoch. Bootstrapping
  it from the previous epoch's root while later roots are missing silently drops checkpoints
  (bug 180155dc1e, §5).

**R2.6 [E] If the change-epoch tx was already executed, the builder stops.** Inside
`create_and_execute_advance_epoch_tx` (AUTH:6187-6202):

- it acquires the tx lock;
- if `is_tx_already_executed(digest)` holds, typically because state sync delivered the certified
  final checkpoint first, it returns `ChangeEpochTxAlreadyExecuted`;
- it returns the same error if the execution lock's epoch no longer matches.

`CheckpointBuilder::run` treats this error, and `SystemPackagesMissing`, as terminal and returns
(CP:1475-1503). Reconfiguration will shut the builder down anyway.

- Why: building over inputs that have already advanced would produce nonsense or panic (bugs
  526418dd77, c70aa33eeb, 3bd67c24ac).
- For the rewrite: the per-epoch builder must tolerate never producing the final checkpoint.
  Startup code must not wait for it to (bug ca5f315f8f, R4.7).

**R2.7 [E] Missing system packages make the builder give up, not halt the node.** Suppose a
framework upgrade was voted but this binary lacks the package bytes, or the binary does not
support the voted protocol version. Then construction fails with `SystemPackagesMissing`
(AUTH:6106-6135). The node then relies on receiving the certified final checkpoint through state
sync and executing it (R3.2). After reconfiguration, `check_protocol_version` exits the process
if the new version is unsupported (R4.20).

### 2.2 The change-epoch transaction

**R2.8 [E] Contents and order.** `create_and_execute_advance_epoch_tx` (AUTH:6033-6249) builds an
`EndOfEpochTransaction` from this ordered list of optional kinds, followed by `ChangeEpoch`
(AUTH:6044-6086, AUTH:6141-6152). Each optional kind is included only when its protocol flag or
object-existence condition holds.

1. `AuthenticatorStateCreate`, or `AuthenticatorStateExpire{min_epoch = next_epoch -
   max_age_of_jwk_in_epochs, initial_shared_version}` (AUTH:5647-5681).
2. `RandomnessStateCreate`.
3. `BridgeStateCreate`, then `BridgeCommitteeInit`.
4. `DenyListStateCreate`.
5. `StoreExecutionTimeObservations`. Its keys are the PTB commands of the final checkpoint's
   transactions (CP:2005-2023), and `last_checkpoint` is the previous built checkpoint seq
   (AUTH:6061-6067).
6. `AccumulatorRootCreate`.
7. `CoinRegistryCreate`, `DisplayRegistryCreate`, `AddressAliasStateCreate`,
   `ForwardingAddressRegistryCreate`.
8. `WriteAccumulatorStorageCost`.
9. `ChangeEpoch{epoch: N+1, protocol_version, storage_charge, computation_charge,
   storage_rebate, non_refundable_storage_fee, epoch_start_timestamp_ms, system_packages}`.

The order is consensus-critical: the digest must match on every validator. The executor asserts
`ChangeEpoch` is last (EXE:1714-1716, EXE:1803). Before `end_of_epoch_transaction_supported`, a
bare `ChangeEpoch` transaction is used (AUTH:6153-6164).

**R2.9 [E] Protocol version selection.** `next_epoch_protocol_version` comes from
`choose_protocol_version_and_system_packages_v2` (AUTH:5621-5645, AUTH:5533-5619), which uses the
capabilities received through consensus this epoch (`get_capabilities_v2`).

- It repeatedly tries `current + 1`. With `advance_to_highest_supported_protocol_version` it can
  go further.
- A candidate version is chosen if one (protocol-config digest, sorted system packages) group has
  stake of at least `quorum_threshold + ceil(f * buffer_stake_bps / 10000)`.
- Validators that list no packages, or do not support the proposed version, vote no.
- `buffer_stake_bps` can be overridden per epoch (`get_effective_buffer_stake_bps`,
  AUTH:6090).
- The chosen system packages are loaded by the voted object refs (`get_system_package_bytes`,
  AUTH:6106-6109). Pinning to the voted versions makes a rebuilt tx byte-identical
  (bug 3bd67c24ac).

**R2.10 [E] Capability advertisement at the start of each epoch.** At the top of every
reconfiguration loop iteration, a validator submits `CapabilityNotificationV2` with
`supported_protocol_versions.truncate_below(current)` and its available system packages
(NODE:1964-2010). If the max supported version would enable accumulators but the accumulator root
object does not exist yet, the advertised max is lowered (NODE:1980-1997). This vote feeds R2.9
in the *same* epoch. A validator that never advertises can never vote for an upgrade.

- [I] The 1 ms sleep before submitting (NODE:1971-1972) is a workaround and has a TODO on it.

**R2.11 [E] Shared-object versions for the change-epoch tx are assigned locally, not by
consensus.** The builder calls `assign_shared_object_versions_idempotent` on the single
transaction (AUTH:6204-6212). The executor derives the versions from the certified effects
(`acquire_shared_version_assignments_from_effects`, CE:1027-1035). The inputs are the system
state object (`SUI_SYSTEM_MUT`, EXE:1887-1888) plus whatever shared system objects the optional
kinds touch (authenticator state, randomness, deny list, bridge, accumulator root, and so on). The
initial shared versions of those objects are captured in the epoch start configuration (ESC:31-41).

**R2.12 [E] Execution semantics (Move side).** `advance_epoch` (EXE:1975-2046) does the following:

1. Mints the storage and computation rewards.
2. Calls `sui_system::advance_epoch(rewards, system_state, epoch, next_protocol_version,
   storage_rebate, non_refundable_storage_fee, storage_fund_reinvest_rate,
   reward_slashing_rate, epoch_start_timestamp_ms)`, then
   `balance::destroy_storage_rebates` (EXE:1877-1926).
3. If the Move call fails, drops its writes, resets storage cost and rebate, and applies
   `advance_epoch_safe_mode` (EXE:2015-2027). Safe mode is a fallback that still advances the
   epoch number and protocol version.
4. Always installs the voted system packages (`process_system_packages`, EXE:2048-2124).
   Version `OBJECT_START_VERSION` means publish a new package. Otherwise the package is upgraded
   in place, with the version decremented first so that effects record +1.

The change-epoch tx runs under the **current** epoch's protocol config and VM. The new framework
takes effect for execution only when the N+1 epoch store builds its execution components from
the new protocol config (EPS:907-916). The builder asserts that the transaction succeeded
(AUTH:6247). A failed change-epoch tx is a bug, not a recoverable error.

**R2.13 [E] The builder never commits the change-epoch tx.** The builder calls
`execute_certificate` and gets `TransactionOutputs`, but does not write them to the cache
(AUTH:6220-6230). It only calls `state_sync_store.insert_transaction_and_effects(tx, effects)`
(AUTH:6236-6239), so that state sync can serve the transaction and effects once the checkpoint is
certified. Objects are written only when the checkpoint executor executes the certified final
checkpoint.

- Why: the validator's locally voted inputs, especially the protocol version, may differ from
  what the network certifies (bug f94b5d6be9).
- [I] Writing the transaction and effects early into the perpetual tables is incidental. Both are
  content-addressed, so it is harmless.

### 2.3 End-of-epoch data and the root state hash

**R2.14 [E] `EndOfEpochData` contents** (CP:2071-2118, MCK:305-320):

- `next_epoch_committee` is
  `system_state_after_advance.get_current_epoch_committee().committee().voting_rights`, that is,
  the N+1 committee read from the *post-execution* system state object in
  `transaction_outputs.written` (AUTH:6231-6232). It is sorted as the `Committee` structure
  sorts it.
- `next_epoch_protocol_version` is `system_state.protocol_version()` after the advance.
- `epoch_commitments` is `[ECMHLiveObjectSetDigest(root_state_hash)]` if
  `commit_root_state_digest` is enabled, otherwise empty (CP:2104-2110).

State sync and light clients use `next_epoch_committee` to verify the first checkpoint of N+1
(SS:899-916). It is the only certified hand-off of the committee.

**R2.15 [E] Root state hash definition.** The epoch's root state hash is an ECMH multiset hash.
It is built as follows:

- Each checkpoint's hash comes from its effects (`accumulate_effects`, GSH:153-370; v3 for
  current protocol versions).
- A running root is kept per checkpoint: `running_root[s] = running_root[s-1] ∪ ckpt_hash[s]`
  (GSH:537-577).
- At the start of the epoch, `running_root[first-1]` is the previous epoch's root, read from
  `root_state_hash_by_epoch[N-1]` (GSH:506-535).
- `accumulate_epoch(epoch, last_seq)` copies `running_root[last_seq]` into the perpetual
  `root_state_hash_by_epoch[N] = (last_seq, hash)` (GSH:579-600).

It is cumulative from genesis, and it equals the hash of the live object set (R4.10).

**R2.16 [E] Both builder and executor compute the epoch root, idempotently.** The builder:

- accumulates the final checkpoint,
- waits for running root `seq-1`,
- calls `accumulate_running_root(seq, Some(acc))`,
- calls `digest_epoch`, which persists the epoch root (CP:2079-2100).

The executor later runs `accumulate_running_root(seq, ...)` again, which is a no-op if the root
already exists (GSH:549-560), and then `accumulate_epoch` again (CE:502-513).

- `accumulate_checkpoint` returns an existing per-checkpoint hash instead of recomputing it
  (GSH:404-406).
- At checkpoint-service startup, state hashes after `max(last_persisted_builder_seq,
  highest_executed)` are cleared (CP:3013-3048). This is so a hash of an uncertified,
  locally built checkpoint is never reused (bugs d829e2b530, 77bebabad1).
- For the rewrite: per-checkpoint state hashes from the builder may be cached, but a hash of a
  checkpoint that was not certified must never survive a restart.

**R2.17 [I] Other consistency checks at the epoch end.** Some checks are not consensus-critical
but help catch bugs early:

- the SUI conservation check (panics in debug, warns in release);
- the optional live-object-set vs root-hash check;
- the optional secondary-index completeness check (AUTH:4094-4183, AUTH:4185-4215, R4.10).

**R2.18 [E] Accumulator settlement at the epoch end works as usual.** Each chunk in the final
commit gets its own `AccumulatorSettlement` schedulable when accumulators are enabled
(CH:1561-1572). Nothing about settlement is specific to the epoch end, apart from
`WriteAccumulatorStorageCost` in the change-epoch tx (AUTH:6084-6086).

---

## 3. Detecting that the epoch is over

### 3.1 Checkpoint executor

**R3.1 [E] The executor consumes certified ("synced") checkpoints in order and stops after the
final one.** `stream_synced_checkpoints` waits for `notify_read_synced_checkpoint(seq)` and ends
the stream right after yielding a checkpoint with `end_of_epoch_data` (CEU:23-73). On validators,
certified checkpoints reach the synced watermark through the aggregator handing them to state
sync (`SendCheckpointToStateSync`, `crates/sui-core/src/checkpoints/checkpoint_output.rs:200-216`,
SS:917-922). On fullnodes they arrive from peers. **This is shared code: validators and fullnodes
run the same `CheckpointExecutor` and the same loop in `monitor_reconfiguration`.**

**R3.2 [E] The final checkpoint is always executed transaction by transaction ("synced path"),
even on validators.**

- Non-final checkpoints on consensus-running nodes take `verify_locally_built_checkpoint`, which
  compares against the locally built summary and reuses the builder's state hash (CE:534-627).
  That path asserts the checkpoint is not final (CE:543-546).
- The final checkpoint takes `execute_transactions_from_synced_checkpoint` (CE:390-399,
  CE:632-664). The builder did not commit the change-epoch tx (R2.13), so it must be executed
  here.

**R3.3 [E] The change-epoch tx executes last, after everything else in the epoch.** In
`execute_checkpoint`:

1. If the checkpoint is final and `seq > 0`, the executor first waits for
   `notify_read_executed_checkpoint(seq-1)` (CE:372-383).
2. Within the final checkpoint it schedules every other unexecuted transaction (CE:913-992),
   waits for their effects (CE:643-650), and only then enqueues the change-epoch tx. The tx is
   enqueued with assigned versions derived from its certified effects and with
   `ExpectedEffectsDigest::Certified`, and the executor waits for it (CE:996-1065).

- Why: the change-epoch tx mutates the system state and framework packages and is not causally
  ordered after the other transactions. Running it early lets later transactions see the next
  epoch's state, or lets its effects be persisted before earlier ones (bug 5f8b3fddc3).
- During crash recovery the change-epoch tx may already have been executed. The executor
  tolerates this; an assert to that effect is deliberately commented out (CE:1013-1025).

**R3.4 [E] Persistence order for the final checkpoint.** After the change-epoch tx executes
(CE:411-529):

1. Build and atomically commit the DB batch of all of the checkpoint's transaction outputs,
   including the change-epoch tx's new system state and packages, together with the
   highest-committed-checkpoint marker (CE:416-462).
2. `epoch_store.handle_finalized_checkpoint`: flush quarantined consensus output up to this
   checkpoint into the epoch DB (CE:470-472, EPS:2057-2089).
3. Notify the randomness reporter (CE:478-492). Enqueue to subscribers or RPC (CE:496-498).
4. `accumulate_running_root(seq)` (CE:502-504).
5. **If final**:
   - `checkpoint_store.insert_epoch_last_checkpoint(N, ckpt)` (CE:507-509). This is a
     duplicate: `insert_certified_checkpoint` already wrote `epoch_last_checkpoint_map`
     (CP:841-846).
   - `accumulate_epoch(N, seq)`, which persists `root_state_hash_by_epoch[N]` (CE:511-513).
   - Prune locally computed summaries (CE:515-518).
6. `bump_highest_executed_checkpoint(seq)`, which asserts strict +1 progression (CE:523,
   CE:1068-1077).

Step 6 is the **durable "epoch N is fully executed" marker**. Restart logic keys off it (R4.3).
Everything before it must be idempotent, because a crash between steps causes the checkpoint to
be re-executed.

**R3.5 [E] `run_epoch` returns `EpochComplete`** when the fold over the checkpoint stream ends on
a final checkpoint (CE:319-349). It returns immediately if the highest executed checkpoint is
already the final checkpoint of the current epoch (`get_next_to_schedule() == None`, CE:250-283,
CE:305-308). `RunWithRangeCondition` is a test and tooling path (CE:297-300, NODE:2056-2062).

**R3.6 [I] Executor lifetime.** A new `CheckpointExecutor` is constructed for every epoch with the
current epoch store and hasher (NODE:1938-1953). It is consumed by `run_epoch` (`self`, CE:286),
so when the call returns, no executor tasks hold the hasher or the epoch store. NODE:2169-2171
relies on this when it calls `Arc::into_inner(hasher)`.

### 3.2 What the validator is doing while waiting

**R3.7 [E] Between the final round and reconfiguration, the validator keeps doing the
following:**

- delivering checkpoint signatures from consensus to the aggregator (R1.4);
- certifying checkpoints;
- running state sync;
- executing checkpoints, including any that arrive from peers.

It schedules no new user transactions. gRPC submissions are rejected with
`ValidatorHaltedAtEpochEnd` (R1.8). Reads continue against the old epoch store.

---

## 4. Reconfiguration

### 4.1 Sequence in `monitor_reconfiguration`

`monitor_reconfiguration` is spawned once at startup with the starting epoch store
(NODE:1080-1086). It loops forever (NODE:1924-2301). The steps per iteration, in exact order:

**R4.1 [I] Iteration setup.** Take the `GlobalStateHasher` out of its mutex, construct the
`CheckpointExecutor`, set the protocol-version metric, and send capabilities (R2.10). Also
reconcile the transaction-deny-config broadcast (NODE:2019-2051, fork-specific).

**R4.2 [E] Run the executor to `EpochComplete`** (NODE:2054).

**R4.3 [E] Read the next epoch's parameters from the object store.** This step:

- calls `get_sui_system_state_object_unsafe()` (NODE:2065-2069), which is only safe at this
  point, after the final checkpoint's outputs are committed;
- debug-asserts the result is not safe mode (NODE:2071-2081);
- converts it to `EpochStartSystemState` (NODE:2093) with: epoch, protocol_version,
  reference_gas_price, safe_mode, epoch_start_timestamp_ms, epoch_duration_ms, and the active
  validators with keys, addresses and voting power
  (`crates/sui-types/src/sui_system_state/sui_system_state_inner_v1.rs:613-641`);
- derives the next committee from it, then asserts `next_epoch == cur_epoch + 1`
  (NODE:2101-2103).

- **Essential:** the new epoch's committee and parameters come from the **executed** on-chain
  state after the change-epoch tx. They do not come from the builder's locally computed system
  state. They should agree with the certified `EndOfEpochData.next_epoch_committee` (which state
  sync already inserted into the committee store, R4.18), but the reference does not cross-check
  them **(uncertain whether any check exists elsewhere)**.

**R4.4 [I] Side notifications.**

- `end_of_epoch_channel.send(system_state)`, used by the fullnode TransactionOrchestrator
  (NODE:2083-2090).
- Recreate the `AuthorityAggregator` for the new committee (NODE:2095-2099).
- Update the p2p and consensus peer addresses for the new validator set, clearing addresses of
  departed validators (NODE:2114-2119, NODE:2819+).
- A `reconfig_delay` failpoint (NODE:2110).

**R4.5 [E] Reconfigure the authority state, before consensus is shut down.**
`validator_components` is locked (NODE:2121), then `reconfigure_state` runs (NODE:2127-2135,
NODE:2312-2366). It:

1. Loads `epoch_last_checkpoint(N)` and asserts it equals `highest_executed_checkpoint`
   (NODE:2322-2335).
2. Builds `EpochStartConfiguration::new(next_system_state, epoch_digest =
   last_checkpoint.digest(), object_store, default_flags)` (NODE:2337-2343). This reads the
   initial shared version of every well-known shared system object from the *current* object
   store, plus whether the bridge committee is initiated (ESC:207-228).
3. Calls `AuthorityState::reconfigure(...)` (§4.2).

The reference has reconfigured state *before* shutting down consensus since 7396e39973 (2023).
Consensus is in `RejectAllTx` and produces no further writes except checkpoint signatures. Its
handler still holds the old epoch store, whose tables stay open until R4.9.

**R4.6 [E/I] Rebuild the per-epoch components.** There are four cases, keyed on whether the node
ran consensus last epoch and whether it does now, using `NodeRole::from_committee` (NR:35-64).

- **Was running consensus** (NODE:2142-2216):
  1. Mark the transaction pool unavailable if no longer a validator (NODE:2137-2141).
  2. `consensus_manager.shutdown()` (NODE:2158). This closes the transaction pool, clears the
     client, stops the Mysticeti authority, and aborts the commit handler (CM:445-505).
  3. Tell the address prober the node left the committee.
  4. Swap in a **new** `GlobalStateHasher` that keeps only the metrics. The old one is asserted
     to have no other strong references (NODE:2167-2177).
  5. `consensus_store_pruner.prune(next_epoch)` (NODE:2179).
  6. If the node still runs consensus, call `start_epoch_specific_validator_components` with the
     **same** consensus adapter, consensus manager, gRPC server handle, overload monitor and
     admission queue (NODE:2183-2208). Otherwise drop the components (NODE:2211-2215).
- **Was not running consensus** (NODE:2217-2275): swap the hasher the same way. If the node now
  runs consensus, call `construct_validator_components`, which builds a new consensus adapter
  and manager, and for validators a gRPC server. Start that server (NODE:2230-2270).

`start_epoch_specific_validator_components` (NODE:1546-1688) creates, for the new epoch store:

- a new `CheckpointService` (builder, aggregator, state hasher task), with
  `SubmitCheckpointToConsensus` output on validators (NODE:1690-1722);
- cleared randomness VSS public key, so round signatures buffer until the new DKG completes
  (NODE:1580-1582);
- a new `RandomnessManager` if randomness is enabled (NODE:1584-1602);
- a spawned `ExecutionTimeObserver` (NODE:1604-1613);
- a new `ConsensusHandlerInitializer` and `SuiTxValidator`. Consensus is started asynchronously
  in a spawned task (NODE:1620-1653);
- the checkpoint service, spawned behind the consensus replay waiter (NODE:1655-1665, R4.15);
- the JWK updater (NODE:1667-1675);
- `admission_queue.rotate_for_epoch` (NODE:1677-1679).

**R4.7 [E] Consensus starts in the new epoch with the new committee and a fresh per-epoch DB.**
`ConsensusManager::start` (CM:262-443) does the following:

- takes the committee from `epoch_store.epoch_start_state().get_consensus_committee()`
  (CM:271-278);
- uses `db_path = get_store_path(epoch)` (CM:335-337);
- resumes after `last_processed_commit_index` from the epoch store, minus
  `consensus_num_requested_prior_commits_at_startup` for replay (CM:341-350);
- increments `boot_counter` only if the previous run handled any commits (CM:365-386);
- passes `epoch_start_timestamp_ms` to `ConsensusAuthority::start` (CM:388-404).

The boot counter is used to avoid amnesia-recovery on a normal epoch change while forcing it after
a restore.

**R4.8 [I] The gRPC validator server is perpetual while the node stays a validator.** Its
`SpawnOnce` handle is passed into the new components unchanged (NODE:2196). The
`ValidatorService` loads the epoch store per request (`load_epoch_store_one_call_per_task`,
SRV:648, SRV:705), so it serves the new epoch automatically. When a validator leaves the
committee, the `ValidatorComponents` are dropped. Dropping a started `SpawnOnce` drops a tokio
`JoinHandle`, which **does not abort** the server task (NODE:2788-2813). So the gRPC server
probably keeps running detached, and a later re-promotion would bind a second server to the same
address **(uncertain; looks like a latent bug, not a requirement)**.

**R4.9 [E/I] Release the old epoch's DB handles after the new components are in place.**
`cur_epoch_store.release_db_handles()` sets `tables = None` (NODE:2280, EPS:1085-1089). After
that, any access through a lingering `Arc<AuthorityPerEpochStore>` gets `EpochEnded`
(EPS:1075-1080). Some paths deliberately ignore `EpochEnded`, for example
`handle_finalized_checkpoint` (EPS:2062-2068).

- Why: old epoch stores outlive the epoch through stray `Arc`s, and they leaked memory
  (bug 18c233534e).
- Essential: no task may read or write epoch N's tables once epoch N+1 is running.
- Incidental: enforcing that by nulling the handle instead of through ownership.

Then `epoch_store = new_epoch_store` and the loop repeats (NODE:2298-2299).

### 4.2 `AuthorityState::reconfigure`

The steps, in order (AUTH:3817-3910):

**R4.10 [E] Order of operations.**

1. `check_protocol_version(supported, new_protocol_version)`. Exits the process if unsupported
   (AUTH:3827-3832, AUTH:3376-3397).
2. `committee_store.insert_new_committee(N+1)` (AUTH:3834). This is idempotent. State sync may
   have inserted it already.
3. Take the **execution write lock** (AUTH:3837, AUTH:3812-3814). This waits for all in-flight
   execution and validation, which hold read locks. New attempts fail fast:
   - validation uses `try_read`, which fails with `ValidatorHaltedAtEpochEnd` (AUTH:3806-3810);
   - execution checks that the lock's epoch equals the certificate's epoch (AUTH:3791-3802).
4. `cur_epoch_store.epoch_terminated()`, which cancels `epoch_alive_token` and then takes the
   `epoch_alive` write lock (AUTH:3840, EPS:2912-2920). That lock is a barrier: it waits for
   every future inside `within_alive_epoch` and every holder of `enter_alive_epoch` to finish
   (EPS:2929-2952). Covered work includes the checkpoint builder (CP:3056-3064), consensus
   adapter submissions, execution-scheduler tasks (ESI:399, ESI:447, ESI:580), the settlement
   scheduler, the execution driver (ED:150-156), effects waits (SRV:1777), and the execution-time
   estimator.
5. `clear_state_end_of_epoch` (AUTH:3848-3849, WBC:1355-1397). Everything still dirty in the
   writeback cache was never in a checkpoint, so the step:
   - invalidates its cached objects and packages;
   - clears the dirty set and the object locks;
   - clears the per-epoch object marker table.

   In this fork every transaction goes through consensus and every scheduled transaction lands in
   a checkpoint, so the dirty set should be empty. A non-empty entry with shared inputs triggers
   `debug_fatal` (WBC:1361-1370). This replaced `revert_uncommitted_epoch_transactions` in
   bca8a306df.
6. `check_system_consistency`: SUI conservation, plus the optional root-hash vs live-object-set
   check and index check (AUTH:3850, AUTH:4094-4183). A root-hash mismatch is `debug_fatal` and
   sets `inconsistent_state` on the hasher (AUTH:4185-4215).
7. `maybe_reaccumulate_state_hash`, a one-off migration for `simplified_unwrap_then_delete`
   (AUTH:3851-3856) [I].
8. **`set_epoch_start_configuration(new_config)`**, written to the perpetual DB (AUTH:3857-3858,
   AST:608-619). **This is the durable commit point of reconfiguration** (R4.21).
9. Optional DB checkpoint (snapshot) of the epoch's DBs (AUTH:3859-3871) [I].
10. `reconfigure_cache`, a no-op now (AUTH:3873-3875, EC:886-894) [I].
11. `reopen_epoch_db`: `cur_epoch_store.new_at_next_epoch(...)`, then
    `self.epoch_store.store(new)`. This is the atomic swap that all
    `load_epoch_store_one_call_per_task` callers observe (AUTH:3878-3886, AUTH:6252-6281,
    EPS:1244-1278).
12. `execution_scheduler.reconfigure(new_epoch_store)`. This rebuilds the address-funds withdraw
    scheduler from the current accumulator root version and closes the old one (AUTH:3888-3889,
    ESI:604-622).
13. `init_object_funds_checker()` (AUTH:3890).
14. Update the transaction-deny-config committee (AUTH:3892-3902) [I, fork-specific].
15. `*execution_lock = new_epoch`, then `notify_epoch(new_epoch)`, which wakes `wait_for_epoch`
    waiters (AUTH:3904-3906, AUTH:3913-3926). The write lock is dropped on return, and execution
    for N+1 resumes.

Why this order matters:

- The execution lock (3) prevents a certificate from being executed against a half-switched
  epoch (bug b376f812b5). It also stops object locks from being cleared under a running
  `handle_transaction` (bug 7c9b1d1f09).
- `epoch_terminated` (4) must come after the execution lock and before clearing state. Old-epoch
  tasks must not observe or produce state across the switch (bugs 68268419cc, 5aea124ff8,
  8d2bb84a1c).
- The epoch start config (8) must be persisted before the new epoch store is used, and the
  recovery epoch must not point at an epoch whose state is not set up (bug 43d8ef5c8c).
- The lock epoch (15) is updated only after the store swap (11). An executor that sees the new
  lock epoch is therefore guaranteed to see the new epoch store (comment at AUTH:3907-3909).

**R4.11 [E] What the new epoch store is built from.** `AuthorityPerEpochStore::new`
(EPS:844-1058) takes the inputs below. Nothing else is carried over in memory.

- the new committee;
- the `EpochStartConfiguration`, from which the protocol config is derived as
  `ProtocolConfig::get_for_version(protocol_version, chain)` (EPS:907);
- `highest_executed_checkpoint = previous_epoch_last_checkpoint = epoch_last_checkpoint(N)`
  (EPS:1270-1274);
- the object store, from which it reads epoch-start state:
  - the active JWKs from the authenticator state object (EPS:950-965);
  - the stored execution-time observations from the system state (EPS:993-1001);
- its own per-epoch tables, for reconfig state, end-of-publish set, consensus stats, and so on
  (EPS:869-875). These are empty for a fresh epoch.

`node_role` is computed from committee membership (EPS:1068, NR:35-47). It asserts the start
config's epoch equals the committee's epoch (EPS:884-887).

**R4.12 [E] `EpochStartConfiguration` contents** (ESC:192-228):

- `system_state`: the `EpochStartSystemState`;
- `epoch_digest`: the digest of epoch N's last checkpoint, zero at genesis (ESC:300-307);
- `flags`: epoch flags. All are currently deprecated or test-only (ESC:92-110);
- `system_object_versions`: a map from each well-known shared system object to its
  `initial_shared_version`, if the object exists (ESC:31-41, ESC:43-57);
- `bridge_committee_initiated`.

It is persisted in the perpetual DB as a single row (AST:608-619). It fully determines which
system transactions the epoch can produce and their shared inputs. It must be computed *after*
the final checkpoint's outputs are committed, because the change-epoch tx may create some of
these objects.

### 4.3 Services during reconfiguration

**R4.13 [E/I] gRPC submit path.** The behavior depends on timing:

- During the close (R1.8): `ValidatorHaltedAtEpochEnd`.
- While `reconfigure` holds the execution write lock: validation fails fast with
  `ValidatorHaltedAtEpochEnd` (AUTH:3806-3810).
- The submit handler retries once after `wait_for_epoch(start+1)`, with a 15 s cap
  (SRV:648-692).
- `wait_for_effects` runs inside `within_alive_epoch` and returns `EpochEnded` when the epoch
  terminates (SRV:1770-1784). The client must re-query in the new epoch.
- Requests that loaded the old store before the swap finish against it, or get `EpochEnded`
  after the handles are released (R4.9). The server never blocks waiting for reconfiguration to
  finish, except inside the bounded retry.

**R4.14 [E] State sync never reconfigures and never stops.** It keeps syncing across the
boundary. When it processes a final checkpoint it inserts the next committee into the committee
store, so it can verify N+1 checkpoints before the local node has reconfigured (SS:899-916,
STOR:352-370). It may run arbitrarily far ahead. The executor executes one epoch at a time
(R3.1). The p2p network, discovery, randomness network and RPC/indexing are perpetual
(NODE:1377-1395).

**R4.15 [E] Checkpoint builder replay gating per epoch.** The new epoch's checkpoint service
waits for consensus to finish replaying already-processed commits before building (CP:1475-1481,
NODE:1655-1665). `CheckpointService::spawn` then waits, with a 120 s soft timeout, for either
"builder finished" or all previously built checkpoints being rebuilt (CP:3076-3089). This matters
on restart (R4.24). At a normal epoch start there is nothing to rebuild.

**R4.16 [I] Read paths.** JSON-RPC, gRPC reads and the subscription service read from perpetual
stores and the current epoch store loaded per call. They keep serving throughout. Fullnode-only
components (the TransactionOrchestrator) switch their aggregator on `end_of_epoch_channel`
(NODE:2083-2090).

### 4.4 Protocol upgrades and committee membership

**R4.17 [E] The protocol version takes effect exactly at the epoch boundary.** It is a field of
the epoch-start system state. Every component that depends on it reads the epoch store's
`protocol_config()`, and the epoch store is rebuilt per epoch (EPS:907).

- Components that captured config at construction are rebuilt per epoch: the consensus handler
  and manager parameters, the checkpoint queue's `max_tx` and interval (CH:812-818), and the
  congestion and execution-time estimator.
- Consensus is restarted with `to_consensus_protocol_config(new_config)` (CM:272).

**R4.18 [E] Committee hand-off.** The N+1 committee may enter the committee store in two ways:
from state sync, on processing the final checkpoint (`EndOfEpochData`, R4.14), and from
`reconfigure` (R4.10 step 2). Both are idempotent inserts of the same committee.

**R4.19 [E] Leaving or joining the committee.**

- **Leaving** (validator to fullnode): the node reconfigures its authority state like everyone
  else, shuts down consensus, and does not restart the per-epoch validator components. From then
  on it follows the chain through state sync and the executor (NODE:2209-2215). Bug ce810b8f3e:
  fullnodes once never reconfigured at all.
- **Joining** (fullnode to validator): the authority state is reconfigured first. Then the full
  validator component set is constructed, including a new consensus adapter and the gRPC server
  (NODE:2230-2270).
- Consensus observers (`FullNode(ConsensusObserver)`) also "run consensus" and get the
  consensus-side components without a gRPC server (NR:57-64, NODE:1004, NODE:1455-1468).

**R4.20 [E] Unsupported protocol version means stop.** At startup and at every reconfiguration,
`check_protocol_version` calls `std::process::exit(1)` (outside simtest) if the epoch's protocol
version is not in `supported_protocol_versions` (AUTH:3376-3397, AUTH:3421, AUTH:3827). The
operator must upgrade the binary. On restart, the node reopens epoch N+1 and repeats the check.

---

## 5. Crash recovery around epoch boundaries

### 5.1 What is persisted, and where

| Item | Store | Written by |
|---|---|---|
| `epoch_start_configuration` (single row) | perpetual | genesis, `reconfigure` step 8 |
| committees by epoch | committee store | state sync, `reconfigure` step 2 |
| certified checkpoints, `epoch_last_checkpoint_map` | checkpoint store | aggregator or state sync (CP:823-848), executor (CE:507) |
| `highest_synced` / `highest_executed` watermarks | checkpoint store | state sync / executor |
| transaction outputs incl. new system state | perpetual | executor batch commit (CE:447-462) |
| `root_state_hash_by_epoch` | perpetual | builder (CP:2097-2099), executor (CE:511) |
| reconfig state, end-of-publish set, consensus stats, pending checkpoints (after quarantine flush), running roots and checkpoint state hashes | epoch_N DB | consensus handler (quarantined) and executor |
| consensus DAG | consensus DB per epoch | Mysticeti |

**R4.21 [E] The current epoch at startup is the epoch of the persisted
`EpochStartConfiguration`.** `get_recovery_epoch_at_restart()` returns
`epoch_start_configuration.epoch_start_state().epoch()` (AST:599-606, NODE:596). The committee is
loaded from the committee store for that epoch, and it must exist (NODE:597-599). The epoch store
is opened from that config (NODE:600-670). `previous_epoch_last_checkpoint` comes from
`epoch_last_checkpoint_map[cur-1]`, falling back to `highest_executed` (NODE:635-643; the
fallback is presumably for snapshot restores, **uncertain**).

Bug 198e543661: the epoch store was once always rebuilt from genesis. Bug 9a8f1bbe0d: the
Narwhal committee was once read from live system state instead of the epoch start config.

### 5.2 Crash windows

**R4.22 [E] Crash before the final checkpoint is fully executed.** On restart in epoch N,
consensus replays, the builder rebuilds, and the executor resumes from `highest_executed + 1`.
This is ordinary recovery.

- If the crash came after the final checkpoint's outputs were committed but before
  `highest_executed` was bumped, the final checkpoint is executed again:
  - transactions with effects already present are checked against the certified digests and not
    re-run (CE:944-950);
  - the change-epoch tx may already be executed (CE:1013-1025);
  - `insert_epoch_last_checkpoint`, `accumulate_running_root` and `accumulate_epoch` are
    idempotent overwrites (CE:502-513, GSH:549-560).
- The builder may find the change-epoch tx already executed and stop (R2.6). The 120 s
  rebuilt-checkpoints wait is satisfied by the "builder finished" branch (CP:3076-3083, bug
  ca5f315f8f).

**R4.23 [E] Crash after `highest_executed = final`, before `set_epoch_start_configuration`.** On
restart:

- The node is in epoch N, with the old config.
- **On validators, consensus for epoch N is started again** at startup (NODE:964-1010), as is the
  checkpoint service.
- `CheckpointExecutor::get_next_to_schedule` sees that the highest executed checkpoint is the
  final checkpoint of the current epoch and returns `None`. `run_epoch` then returns
  `EpochComplete` immediately (CE:256-266, CE:305-308; bug 4caaa72e3f).
- Reconfiguration proceeds from R4.3 with the committed system state.
- `reconfigure` steps 2 to 7 are re-run and are idempotent. Step 9 (the DB checkpoint) skips if
  it already exists and removes a stale tmp dir (AUTH:4228-4246).

**R4.24 [E] Crash after `set_epoch_start_configuration`.** On restart the node is in epoch N+1.

- The N+1 committee was inserted at step 2, before the commit point.
- The epoch N+1 store is opened, empty or partially populated.
- Everything after step 8 in `reconfigure`, and all of the node-level component setup, is
  in-memory or idempotent: the scheduler, funds checker, consensus start with a new DB path, and
  the old consensus DB pruning.
- The epoch-N DB checkpoint (step 9) may be skipped permanently [I].
- `check_system_consistency` for epoch N does not re-run [I].
- The executor starts at `epoch_last_checkpoint(N) + 1`, because the highest executed checkpoint
  is from a different epoch (CE:256-282).

**R4.25 [E] Crash mid-epoch N+1, early.** This is ordinary recovery. Consensus restarts from the
epoch's DB and replays from the last processed commit (CM:341-350). The checkpoint builder waits
for replay, then rebuilds locally built checkpoints that were lost from the quarantine
(CP:1475-1481, CP:3000-3090). Consensus-output quarantine means epoch-DB state for commits
beyond `highest_executed` is recomputed by replay, not read from disk (EPS:1029-1032,
EPS:2077-2079).

**R4.26 [I] `recover_end_of_publish` at startup** resubmits `EndOfPublish` if the node crashed
after persisting `RejectUserCerts` (NODE:986-989, CA:317-325). This applies to manual close only,
so the rewrite can skip it.

**R4.27 [E] Invariants a reimplementation must keep for restart correctness.**

1. The epoch-start config is written only after:
   - the final checkpoint's outputs are durable,
   - `highest_executed == final` is durable,
   - the next committee is durable.
2. A node whose `highest_executed` is the final checkpoint of its recorded epoch must go straight
   to reconfiguration without waiting for anything else in that epoch. That includes not waiting
   for the builder to produce the final checkpoint.
3. Everything between "final checkpoint executed" and "epoch-start config written" must be
   idempotent and re-runnable.
4. Nothing that depends on uncertified local work survives a restart: builder state hashes past
   the last committed checkpoint are cleared at startup (CP:3013-3048), and quarantined consensus
   output is replayed.

---

## 6. Bug history

These are the relevant fixes, oldest first. Several predate the writeback cache and the removal of
fastpath, so their exact mechanisms no longer exist. The lesson for each still applies.

### 6.1 Ordering inside reconfiguration

- **ce810b8f3e** (2022-12, #6684). `monitor_reconfiguration` reconfigured state only on
  validators, so fullnodes never moved epochs. Fix: every node reconfigures its state, and
  validator promotion and demotion are handled afterwards.
- **f32eb00371** (2022-12, #6840). Every epoch store shared one WAL directory, so opening N+1
  while N was open clashed. Fix: the per-epoch path. Lesson: per-epoch on-disk state must be
  namespaced by epoch (EPS:785-787).
- **9feec01efa** (2023-01, #7144). State was reconfigured before Narwhal stopped. Fix: stop
  consensus first. *Reversed later* (7396e39973): today the state is reconfigured first, which is
  safe because the consensus handler is in `RejectAllTx` by then (R4.5).
- **43d8ef5c8c** (2023-02, #8083). The recovery epoch was persisted before the new epoch store
  was initialized, so a crash in between restarted into a broken epoch. Fix: persist it later.
  Today `set_epoch_start_configuration` plays this role (R4.10 step 8).
- **b376f812b5** (2022-12, #7054). Reverting raced with certificate execution, and old-epoch
  certificates executed after the switch. Fix: an epoch-tagged execution RwLock (R4.10 step 3).
- **7c9b1d1f09** (2024-09, #19320). `clear_locks()` at reconfig ran concurrently with
  `handle_transaction` acquiring object locks, which panicked. Fix: hold the reconfig lock while
  handling transactions.
- **1eddd28580** (2023-01, #7483) and **ce8452129a** (2024-04, #17211). The TransactionManager
  (now the execution scheduler) kept or accepted old-epoch work, and reconfig could interleave
  between two critical sections of `enqueue_impl`. Fix: reset on reconfigure, ignore wrong-epoch
  operations, and hold the lock across the whole operation.

### 6.2 Work outliving its epoch

- **68268419cc** (2022-12, #6923). Consensus adapter tasks outlived the epoch and injected
  old-epoch messages. Fix: `epoch_terminated` waits for them (`within_alive_epoch`).
- **f4af54f2fc** (2023-01, #7111) and **b47f31f2fe** (2023-01, #7451). Components fetched
  `state.epoch_store()` after the swap and ended up on the wrong epoch, including waiting on the
  *new* epoch's termination, which hung. Fix: capture the epoch store once and pass it
  explicitly. **c748f4b478** (2026-01) fixed the same thing in `SuiTxValidator`.
- **1c75247c04**, **8d2bb84a1c**, **5102c1bc46**, **53cccd8a54**, **8fc94e52b2** (2024-09 to
  2025-09). A long sequence about shutting down the checkpoint service:
  - the builder ran past the epoch end and panicked;
  - reconfig did not wait for it, so it used dropped DB handles;
  - draining it could hang because peers had stopped consensus;
  - its loop had no await point, so abort never took effect, and `yield_now` was added
    (CP:1534-1536);
  - `abort_all` killed the hasher before the builder.

  Final design: the builder runs inside `within_alive_epoch`, the hasher task ends when the
  builder's sender drops, and the aggregator is aborted last (CP:3056-3074).
- **adde1a9908**, **ac10df6e7c** (2024-10). The execution driver retried a previous epoch's
  certificate after reconfig and panicked. Fix: an alive-epoch guard and refusing prior-epoch
  certificates (ED:150-156).
- **5aea124ff8**, **0471c68e33** (2024-10, 2025-06). `notify_read_effects` on a transaction that
  might be reverted waited across reconfig, giving a "digests must exist" panic. Fix:
  `epoch_terminated` after the execution lock, and effects waits wrapped in `within_alive_epoch`
  (SRV:1777).
- **18c233534e** (2023-12, #15310). Old epoch stores leaked memory through stray `Arc`s. Fix:
  `release_db_handles` and `EpochEnded` errors (R4.9).

### 6.3 Reverting non-final state

- **ca1a5eb0ab** (2022-12, #6951), **326aebe50e** (#7053), **4c780d87db** (#7255),
  **41cc3aee96** (#9242), **e52af12860** (#12291), **000a9239ea** (#14850). A series about
  reverting locally executed transactions that never reached a checkpoint. Revert had to skip
  transactions that were never executed and transactions that were final through state sync. It
  had to keep the transaction and effects records. Ignored transactions must not be marked
  processed, or revert would miss them.
- **bca8a306df** (2025-08, #23204) replaced all of this: `clear_state_end_of_epoch` drops the
  whole dirty set. With the writeback cache, "dirty at epoch end" means "not in any checkpoint"
  (R4.10 step 5).
- Lesson for the rewrite: if nothing is ever executed outside checkpoint order, there is nothing
  to revert. The guard is still worth keeping.

### 6.4 The final checkpoint and the change-epoch tx

- **f94b5d6be9** (2023-02, #8377). The builder committed its own change-epoch tx, whose voted
  fields could differ from the certified one. Fix: build it and execute it to effects without
  committing (R2.13).
- **5f8b3fddc3** (2023-03, #8941). The change-epoch tx could execute or persist before other
  transactions of the epoch. Fix: the executor holds it back until everything else has executed
  (R3.3).
- **526418dd77** (2023-03, #9497), **c70aa33eeb** (2025-07, #22811), **3bd67c24ac** (2026-07,
  #27439). The builder raced with state sync executing the change-epoch tx. Fixes:
  - check `is_tx_already_executed` under the tx lock and stop;
  - handle that case gracefully;
  - pin system-package lookups to the voted versions, so the rebuilt tx is byte-identical even
    after the packages' `previous_transaction` has changed (R2.6, R2.9).

  The `change_epoch_tx_delay` and `reconfig_delay` failpoints reproduce these races.
- **0649dffda0** (2024-04, #17324). End-of-epoch transactions were filtered out before zipping
  with effects digests, which misaligned every later pair. Lesson: filter after pairing
  (CE:924-951).
- **752fc35fe0** (2024-10, #19976). When DKG had failed, the final round marked the user-tx
  pending checkpoint final and then wrote a second, randomness checkpoint, which produced two
  end-of-epoch checkpoints. Fix: mark only the last pending checkpoint (R1.6).
- **3e009ea040** (2023-01, #7299) and **17fe36437a** (2023-01, #7368). Moving builder indexes
  into the per-epoch store reset them, so transactions from epoch N were re-included in N+1. The
  previous checkpoint across the boundary was read from a table that could be stale or empty.
  Fix: a permanent executed index, and reading the previous epoch's last checkpoint from the
  certified store (CP:1883-1906).
- **cea8742e81** (2024-01, #15287) and **5828f40106** (2025-02, #21376). The epoch could close
  with deferred transactions still unscheduled. The deferred check read an in-memory map that
  had already dropped the loaded keys. Fix: the `RejectAllCerts` state that drains deferred
  transactions, and delaying deferral deletion to the end of the commit handler (CH:1252,
  CH:1359-1368).
- **0bd5fd7e3c** (2026-07, #27300). Stuck deferred transactions left the network half-closed
  (rejecting user transactions but unable to advance), which caused outages. Fix: a
  consensus-derived close deadline. Stragglers are dropped, not cancel-executed, because
  per-validator deferred state cannot be trusted on that path (R1.3).
- **40781b6b39** (2026-01). Object-fund withdraw settlement ran in the checkpoint builder. If the
  network finished the epoch without this builder, settlement never ran and the scheduler lost
  liveness. Fix: settle after execution. Lesson: the builder may not run for the final
  checkpoint, so it must not own any correctness-critical side effect.

### 6.5 Root state hash at the boundary

- **d829e2b530** (2023-04, #10160). After crashes, first-write-wins idempotency kept a hash of an
  *uncertified* locally built checkpoint, the root hash failed to match the live set, and the node
  panicked at the epoch end. Fix: remove first-write-wins. Today stale hashes are cleared at
  startup (R2.16).
- **180155dc1e** (2024-06, #18131). A validator behind on execution built the final checkpoint and
  bootstrapped the running root from the previous epoch's root, skipping every checkpoint of the
  epoch but the last. Fix: wait for running root `seq-1` (R2.5).
- **ac3028501a** (2024-06, #18292). A single-checkpoint epoch waited on `seq-1`, which belongs to
  the previous epoch, forever. Fix: early return when `seq-1` is the previous epoch's last
  checkpoint (GSH:488-498).
- **a880a2d5f9** (2024-07, #18568). Epoch 0's final checkpoint could be proposed before genesis
  was accumulated. Fix: rely on the executor always accumulating checkpoint 0 (GSH:500-502).
- **77bebabad1** (2026-01, #24998). A crash after the builder summary was persisted but before
  `highest_executed` was bumped. Startup cleared the state hash, and the executor waited forever
  for a hash the builder would never recompute. Fix: clear only after
  `max(last_persisted_builder_seq, highest_executed)` (CP:3013-3048).

### 6.6 Startup and crash windows

- **198e543661** (2023-01, #7225). The epoch store was always built from genesis. Fix: recover
  the epoch from the perpetual store (R4.21).
- **9a8f1bbe0d** (2023-01, #7452). After the final transaction executed but before reconfig, the
  live system state was at N+1, so startup ran Narwhal for N+1 while Sui was at N. Fix (later
  made structural): consensus is configured from the epoch-start config, never from live system
  state.
- **4caaa72e3f** (2024-03, #16778). `highest_executed` was bumped to the final checkpoint, then
  the node crashed before reconfig, and on restart the executor asserted. Fix: return
  `EpochComplete` immediately (R3.5, R4.23).
- **ca5f315f8f** (2025-09, #23392). The builder may exit without building the final checkpoint,
  so the startup wait for rebuilt checkpoints always timed out. Fix: also finish when the builder
  finishes (R4.22).
- **7f2e4ba70b** (2026-06, #26921). A submit retry asserted that the epoch advanced by exactly
  one, but several reconfigurations can happen before the retry wakes. Fix: accept `>=`
  (SRV:676-687).
- **b8ace6ff5e** (2022-12, #7062). The executor could schedule past the epoch's last checkpoint.
  Fix: the stream stops at `end_of_epoch_data` (CEU:66-71).

### 6.7 Liveness at close (mostly EndOfPublish-era)

- **ae9c95d47b** (2023-05, #12223). EndOfPublish was blocked behind stalled submissions. Fix:
  submit everything at close.
- **dc099596ed** (2023-12, #15216). After certs closed, system transactions were classified as
  ignored like user certs, and randomness rounds were recorded before the ignore check. Fix: a
  separate ignored-system category and recording after the check **(exact symptom uncertain)**.

### 6.8 Recurring lessons

1. **Capture the epoch store once per task.** Anything that re-reads the "current" epoch store
   can straddle the swap.
2. **Every per-epoch task needs a cancellation point and a barrier.** Reconfig must cancel the
   task and *wait* for it. Tasks must actually yield so they can be cancelled. Shutdown order
   matters when tasks feed each other.
3. **The builder and state sync race for the final checkpoint.** Only certified output is ever
   committed. The builder must tolerate being made redundant and must not own side effects.
4. **Each separately persisted marker creates a crash window.** The reference survives with one
   durable commit point (the epoch-start config) and makes everything before it idempotent.
5. **Accumulator edge cases cluster at the boundaries**: the first checkpoint of an epoch, a
   single-checkpoint epoch, genesis, nodes far behind on execution, and restart after uncertified
   work.
6. **Closing must be live under bugs.** A deterministic deadline beats waiting forever.

---

## 7. Essential vs. incidental

### 7.1 What any correct implementation must do

**Agreement (consensus-critical, must match other validators byte for byte):**

- E1. Exactly one final checkpoint per epoch. It is produced from the final consensus commit, is
  last among that commit's checkpoints, and gets `last_of_epoch` (R1.6).
- E2. The final checkpoint's contents are the resolved transactions of its roots plus a trailing
  change-epoch tx. The change-epoch tx has the exact kind list and order of R2.8, the epoch gas
  totals (R2.3), `epoch_start_timestamp_ms` equal to the final checkpoint's timestamp (R2.4), and
  the protocol version and packages chosen by the capability vote (R2.9).
- E3. `EndOfEpochData` = (the N+1 committee from the post-advance system state, the N+1 protocol
  version, `[root_state_hash]` when enabled) (R2.14). The root state hash is the cumulative ECMH
  over all effects through the final checkpoint, seeded from the previous epoch's root (R2.15).
- E4. Shared-object versions for the change-epoch tx are assigned locally and deterministically
  (R2.11).
- E5. Capabilities are advertised every epoch (R2.10).

**Safety (local):**

- E6. The change-epoch tx's outputs are never committed except by executing the certified final
  checkpoint (R2.13).
- E7. The change-epoch tx executes only after every other transaction of the epoch has executed
  (R3.3). The final checkpoint is executed only after `seq-1` has executed.
- E8. The parameters of epoch N+1 (committee, protocol version, timestamps, initial shared
  versions of system objects) are derived from the object store state *after* the final
  checkpoint commits (R4.3, R4.12).
- E9. Before epoch N+1 processing starts:
  - all epoch-N execution and validation has drained,
  - every epoch-N task has been cancelled and joined,
  - uncommitted (non-checkpointed) state is discarded,
  - no component retains write access to epoch-N per-epoch state (R4.10 steps 3-5, R4.9).
- E10. The epoch store swap and the execution epoch switch are ordered so that anything executing
  for N+1 sees the N+1 store (R4.10 step 15).
- E11. A binary that does not support the new protocol version stops instead of executing under
  the wrong rules (R4.20).

**Durability and restart:**

- E12. There is one durable commit point for "now in epoch N+1": the epoch-start config. It is
  written after the final checkpoint's outputs, `highest_executed == final`, and the N+1
  committee are durable (R4.27).
- E13. On startup, the recorded epoch's final checkpoint being already executed means
  reconfiguring immediately without any other wait (R4.23). Everything before the commit point
  is idempotent.
- E14. Uncertified local work (builder state hashes, quarantined consensus output) does not
  survive restarts (R2.16, R4.25).

**Liveness:**

- E15. Checkpoint signatures keep flowing after the final round, so the final checkpoint can be
  certified (R1.4).
- E16. A node that cannot or need not build the final checkpoint (it is behind, lacks packages,
  or state sync got there first) still reconfigures. It gets the final checkpoint from state
  sync and executes it (R2.6, R2.7, R3.1).
- E17. State sync keeps running across the boundary and learns the next committee from the
  certified `EndOfEpochData` (R4.14).
- E18. Clients get a retryable error during the close and the switch, not a hang (R1.8, R4.13).

### 7.2 Artifacts of the reference's structure

- I1. **No explicit component lifetimes.** Perpetual and per-epoch objects are tangled together:
  - The consensus adapter, consensus manager, gRPC server handle, overload monitor and admission
    queue are built once and threaded through every epoch's `ValidatorComponents`.
  - The checkpoint service, randomness manager, consensus handler and JWK updater are rebuilt.
  - The hasher is rebuilt but must be proven unshared with `Arc::into_inner` (NODE:2167-2177).
  - Four branches handle validator and fullnode transitions (NODE:2142-2275).
  - Per-epoch cleanup is enforced after the fact by `release_db_handles` and `EpochEnded` errors
    (R4.9), and by `within_alive_epoch` sprinkled across many modules (R4.10 step 4).
  - Many bugs in §6.2 come from this.

  The rewrite's plan addresses it directly:
  - **Perpetual**: the gRPC server, the object store with its writeback cache, state sync, the
    p2p network, and RPC.
  - **Per-epoch**: the epoch store, consensus, the consensus handler, the checkpoint builder and
    aggregator, the scheduler and executor, randomness, and observers.
  - A loop builds the per-epoch set from the epoch-start config, waits for "final checkpoint
    executed", then tears the set down by ownership (joining every task) before building the
    next one.
- I2. **The reconfiguration order** between "authority state reconfigure" and "consensus
  shutdown" (R4.5). Either order works as long as E9 holds. The reference flipped it once
  (9feec01efa, then 7396e39973).
- I3. **The `ArcSwap` global epoch store** with `load_epoch_store_one_call_per_task` discipline
  (AUTH:4270-4278). An owned per-epoch context passed down explicitly removes the need for it.
- I4. **An epoch-tagged `RwLock<EpochId>` as the execution lock** (AUTH:3791-3814). Any mechanism
  that drains and fences execution works.
- I5. **The builder executes the change-epoch tx to effects and writes the tx and effects into
  the state-sync tables** (AUTH:6236-6239). Only the "do not commit objects" part is essential.
- I6. **Both builder and executor compute the epoch root hash and write
  `root_state_hash_by_epoch`** (R2.16), and both write `epoch_last_checkpoint_map` (R3.4). One
  writer each would do, provided restart idempotency holds.
- I7. **`EndOfPublish`, `RejectUserCerts`, `close_epoch`, `recover_end_of_publish`** are a
  manual-close remnant (R1.1, R4.26).
- I8. **DB checkpoints at the epoch end, the SUI conservation check, the live-set consistency
  check, the secondary-index check and `maybe_reaccumulate_state_hash`** (R2.17, R4.10 steps
  6-7, 9) are operational or migration aids.
- I9. **`EpochFlag`s and the versioned `EpochStartConfiguration` enum** (V1 to V11). A new
  implementation needs only the V11 fields: system state, epoch digest, system object initial
  versions, bridge flag. Backward-compatible decoding is needed only if it must read reference
  databases.
- I10. **Consensus started asynchronously and the checkpoint service gated by a "replay waiter"
  with a 120 s soft timeout** (NODE:1640-1665, CP:3076-3089). The essential property is that
  rebuilt checkpoints are deterministic and nothing waits on a checkpoint the builder will never
  build.
- I11. **The capability-advertisement sleep** (NODE:1971-1972), **the `end_of_epoch_channel`
  broadcast** (fullnode orchestrator), **recreating the `AuthorityAggregator`**, the **address
  prober** and **endpoint manager** updates (R4.4). These matter only to components outside the
  validator core.
- I12. **The gRPC server's `SpawnOnce` that is never aborted** on committee exit (R4.8). This is
  probably a latent bug. The rewrite should make the server perpetual and gate its handlers on
  current role and epoch instead.

### 7.3 A minimal loop for the rewrite (derived, not reference code)

```
startup:
  cfg   = perpetual.epoch_start_configuration          # R4.21
  epoch = cfg.epoch
  loop:
    ctx = EpochContext::open(cfg, committee_store[epoch], perpetual, cache)   # R4.11
    start per-epoch tasks (consensus iff in committee, builder, aggregator, executor, ...)
    wait until executor reports "final checkpoint of `epoch` executed"          # R3.4-R3.5
      (returns immediately if highest_executed is already that checkpoint)      # R4.23
    stop accepting submissions for `epoch` (retryable error)                    # R4.13
    cancel + join every per-epoch task; drain execution                         # E9
    discard uncommitted cache state                                             # R4.10.5
    sys   = read SuiSystemState from object store                               # R4.3
    check protocol version supported, else exit                                 # R4.20
    committee_store.insert(sys.committee)                                       # R4.10.2
    cfg   = EpochStartConfiguration::new(sys, last_ckpt.digest, object_store)   # R4.12
    perpetual.write(epoch_start_configuration = cfg)   # durable commit point   # R4.10.8
    drop ctx (closes epoch-N DB handles)                                        # R4.9
    epoch += 1
```

Perpetual services (gRPC, state sync, object store and cache, p2p, RPC) run outside this loop.
They resolve the current `EpochContext` per request. They see either the old context or the new
one, never a half-built one, and get a retryable error during the gap.

---

## 8. Open questions and uncertainties

1. Whether anything cross-checks the committee read from the executed system state (R4.3)
   against the certified `EndOfEpochData.next_epoch_committee` that state sync inserted. I found
   no such check. Both inserts go through `insert_new_committee`, and I did not verify whether it
   rejects a conflicting committee for an existing epoch.
2. Whether the gRPC server really keeps running after a validator leaves the committee (R4.8).
   Tokio `JoinHandle` drop semantics suggest it does.
3. The `previous_epoch_last_checkpoint` fallback to `highest_executed` at startup (NODE:635-643)
   is presumably for snapshot or formal restores where `epoch_last_checkpoint_map` is missing.
4. Commits after the final round are not pushed to the quarantine (R1.4), so after a restart
   they are replayed again. I believe this is harmless because they produce no state, but I did
   not trace whether checkpoint signatures from them are re-delivered correctly in every case.
