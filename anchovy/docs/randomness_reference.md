# Sui On-Chain Randomness (Random Beacon): Behavioral Reference

This document specifies how the reference Sui validator runs the random beacon: the per-epoch DKG
through consensus, the randomness rounds the commit handler reserves, the threshold signatures
exchanged over anemo, and the `RandomnessStateUpdate` transaction and its checkpoint placement. It
is written so that anchovy can reproduce the parts that decide checkpoint contents, and tell which
parts are local.

Requirements are numbered (R<section>.<n>) and tagged as in `reconfiguration_reference.md`:
**[E]** essential (needed for agreement/no fork, safety or liveness), **[I]** incidental, **[E/I]**
essential property, incidental mechanism. Uncertain statements are marked **(uncertain)**.
`file:line` references are relative to the repo root at commit `c3afaa9cb5`; `anchovy/...` paths
are relative to the anchovy worktree; `TBLS:` and `NODES:` are files of the `fastcrypto-tbls`
crate at the pinned rev `076bf5ad` (`Cargo.toml:630`; in `~/.cargo/git/checkouts/fastcrypto-*/076bf5a/`).

| Abbrev | File |
|---|---|
| PC | `crates/sui-protocol-config/src/lib.rs` |
| SNAP | `crates/sui-protocol-config/src/snapshots/sui_protocol_config__test__version_138.snap` (Chain::Unknown) |
| RND | `crates/sui-core/src/epoch/randomness.rs` |
| CH | `crates/sui-core/src/consensus_handler.rs` |
| EPS | `crates/sui-core/src/authority/authority_per_epoch_store.rs` |
| CQ | `crates/sui-core/src/authority/consensus_quarantine.rs` |
| TD | `crates/sui-core/src/authority/transaction_deferral.rs` |
| SOVM | `crates/sui-core/src/authority/shared_object_version_manager.rs` |
| RRR | `crates/sui-core/src/randomness_round_receiver.rs` |
| CKP | `crates/sui-core/src/checkpoints/mod.rs` |
| CKE | `crates/sui-core/src/checkpoints/checkpoint_executor/mod.rs` |
| AUTH | `crates/sui-core/src/authority.rs` |
| CV | `crates/sui-core/src/consensus_validator.rs` |
| NET | `crates/sui-network/src/randomness/mod.rs` |
| NETB | `crates/sui-network/src/randomness/builder.rs` |
| P2P | `crates/sui-config/src/p2p.rs` |
| NODE | `crates/sui-node/src/lib.rs` |
| MC | `crates/sui-types/src/messages_consensus.rs` |
| TX | `crates/sui-types/src/transaction.rs` |
| CRY | `crates/sui-types/src/crypto.rs` |
| MCK | `crates/sui-types/src/messages_checkpoint.rs` |
| EXE | `sui-execution/latest/sui-adapter/src/execution_engine.rs` |
| RMOVE | `crates/sui-framework/packages/sui-framework/sources/random.move` |
| GEN | `crates/sui-genesis-builder/src/lib.rs` |
| TBLS | `fastcrypto-tbls/src/dkg_v1.rs` |
| NODES | `fastcrypto-tbls/src/nodes.rs` |

---

## 0. Overview

1. Randomness is on when `random_beacon` is set **and** the `Random` object (`0x8`) existed at the
   epoch's start (`randomness_state_enabled`, EPS:1105-1113). Genesis creates it when the flag is
   on; otherwise the first epoch-end with the flag creates it (`RandomnessStateCreate`).
2. **DKG, once per epoch, through consensus.** At epoch start each validator submits a
   `RandomnessDkgMessage` (its VSS polynomial commitment and shares encrypted to every party's
   protocol BLS key). Once the processed messages carry weight ≥ t, each party submits a
   `RandomnessDkgConfirmation` (its complaints). Once confirmations carry weight ≥ 2t−1, DKG
   completes: the group public key `vss_pk` is the sum of the surviving messages' commitments.
   If DKG has not completed by a commit with leader round > 3000, it fails for the epoch.
3. **Rounds, per commit.** Every commit after DKG completes, whose timestamp is ≥ 500 ms after the
   last round's commit, reserves the next `RandomnessRound` (0, 1, 2, … per epoch). Randomness-using
   transactions are deferred until a commit that reserves a round, and then scheduled in a separate
   checkpoint chunk headed by `RandomnessStateUpdate(epoch, round)`, placed after the commit's
   regular checkpoint is flushed.
4. **Bytes, over anemo.** Each validator holding shares partial-signs `"random_beacon round " ||
   bcs(round)` and pushes the partial signatures (later the full signature) to all committee
   peers over the p2p `Randomness` service. Any t-weight of partials aggregates into the unique BLS
   threshold signature, verified against `vss_pk.c0()`. `random_bytes = bcs(signature)`.
5. Everything in checkpoints is a function of consensus output plus the unique signature (R5.8).
   Which messages a validator processes, when DKG completes or fails, and `vss_pk` are computable
   without any private key (R3.9).

---

## 1. Protocol config

### R1.1 [E] Knobs at `ProtocolVersion::MAX` (138), Chain::Unknown

| Config | Value (SNAP line) | Where set | Use |
|---|---|---|---|
| `random_beacon` (feature flag, PC:629) | true (37) | v32 devnet (PC:3350), v48 non-mainnet (PC:3556), v54 all (PC:3716) | enables everything here |
| `random_beacon_reduction_allowed_delta` | 800 (508) | v30 (PC:3318) | weight reduction precision loss bound (R3.2) |
| `random_beacon_reduction_lower_bound` | 500 (509) | 1600 v32/v48, 1000 v54, 800 v57 (PC:3750), 700 v61 (PC:3778), 500 v68 (PC:3839) | min reduced total weight (R3.2) |
| `random_beacon_dkg_timeout_round` | 3000 (510) | v32/v48/v54 | DKG fails after this leader round (R3.7) |
| `random_beacon_min_round_interval_ms` | 500 (511) | 150 v32, 200 v48, 500 v54 (PC:3719) | min commit-timestamp gap between rounds (R4.1) |
| `random_beacon_dkg_version` | 1 (512) | v51 (PC:3612); `dkg_version()` defaults to 1 (PC:2349-2352) | only V1 messages accepted (R3.4) |
| `cancel_for_failed_dkg_early` | true (141) | v96 (PC:4342); asserted (CH:773) | DkgFailed cancel before congestion (R4.5) |
| `always_advance_dkg_to_resolution` | true (142) | v127 (PC:4643) | advance DKG every pending commit (R3.6) |
| `include_cancelled_randomness_txns_in_prologue` | true (159) | v104 (PC:4421); asserted (CH:777) | prologue lists DkgFailed cancellations |
| `split_checkpoints_in_consensus_handler` | true (171) | asserted (CH:763-766) | chunked pending checkpoints (R5.6) |
| `merge_randomness_into_checkpoint` | true (178) | v119 (PC:4542); asserted (CH:779) | randomness chunks join the next flushed checkpoint (R5.6) |
| `max_accumulated_randomness_txn_cost_per_object_in_mysticeti_commit` | 7_400_000 (527) | v69 | randomness tracker debt decay (R4.6) |
| `randomness_scalar` (in `ExecutionTimeEstimate`) | 20 (64) | | randomness tracker budget % (R4.6) |
| `checkpoint_summary_version_specific_data` | 1 (523) | | `randomness_rounds` in the summary (R5.7) |
| `min_checkpoint_interval_ms` | 200 (522) | | checkpoint queue flush interval (R5.6) |

The randomness knobs are not chain-gated after v54, so mainnet and testnet have the same values
at 138. **[E]**: every value enters agreement.

---

## 2. The `Random` object

### R2.1 [E] Creation

- Genesis: if `random_beacon` at the genesis version, the genesis PTB calls `0x2::random::create`
  (GEN:1082-1090); genesis asserts the object exists iff the flag is on (GEN:323-326).
- Otherwise `create_randomness_state_tx` (AUTH:5684-5700) adds
  `EndOfEpochTransactionKind::RandomnessStateCreate` to the epoch's last transaction when the flag
  is on and the object did not exist at the epoch's start; it is the second kind, after
  `AuthenticatorStateCreate` (AUTH:6046-6051). Execution calls `random::create` (EXE:1742-1745).
  Anchovy already does this (`anchovy/crates/checkpoint/src/close.rs:189-190`).
- `create` initializes `epoch = ctx.epoch(), randomness_round = 0, random_bytes = []`
  (RMOVE:42-55).

### R2.2 [E] Initial shared version

`randomness_obj_initial_shared_version()` (`crates/sui-core/src/authority/epoch_start_configuration.rs:266-268`)
reads the object's `Owner::Shared { initial_shared_version }` as of the epoch start. Randomness is
off in the epoch where the object is created (R0.1). For genesis-created objects this is the
genesis version **(uncertain: 1)**; read it from the store.

---

## 3. DKG

### R3.1 [E] Parties

`randomness_dkg_info_from_committee` (RND:993-1026): one party per committee member, with
`id = committee.authority_index(name)` (u16), weight = voting power (out of 10,000), and ECIES
public key = the member's **protocol** BLS public key bytes (G2) as `ecies_v1::PublicKey`.
`authority_info` maps each name to `(PeerId, PartyId)`, with the PeerId taken from the epoch start
state's `narwhal_network_pubkey` (the Ed25519 network key) (RND:375-391,
`crates/sui-types/src/sui_system_state/epoch_start_sui_system_state.rs:236-245`).

A validator's DKG private key is its protocol BLS secret key bytes as a G2 scalar (RND:122-130;
key from `config.protocol_key_pair()`, NODE:1585-1589). Non-validators running consensus get the
`Observer` role (RND:101-176, NODE:1584-1602).

### R3.2 [E] Weight reduction

`Nodes::new_reduced(nodes, t = committee.validity_threshold() = 3334, allowed_delta = 800,
lower_bound = 500)` (RND:395-411, NODES:211-263): the largest `d ∈ 2..=40` such that
`Σ ⌊w_i/d⌋ ≥ lower_bound` (the loop stops at the first `d` below it) and `Σ (w_i mod d) ≤
allowed_delta`. New weights `⌊w_i/d⌋`, new threshold `t' = ⌈3334/d⌉`. A party of weight 0 sends
no message (`IgnoredMessage`, RND:586-592) and its confirmation is ignored.

`Party::new` requires `t' ≤ W'/2` (TBLS:247-250). The random oracle is
`RandomOracle::new("dkg {hex(chain_identifier)} {epoch}")` (RND:413-417).

**Worked example, the compat committee** (voting power 3334/3334/3332): `d = 19` (`d = 20` gives
498 < 500), weights 175/175/175, `W' = 525`, `t' = 176`, `2t'−1 = 351`. The two sui validators have
350 < 351.

### R3.3 [E] Messages through consensus

| Kind (MC:501-509) | Variant index | Payload | Dedup key (MC:784-789) |
|---|---|---|---|
| `RandomnessDkgMessage(AuthorityName, Vec<u8>)` | 6 | `bcs(VersionedDkgMessage::V1(dkg_v1::Message))` | `RandomnessDkgMessage(authority)` |
| `RandomnessDkgConfirmation(AuthorityName, Vec<u8>)` | 7 | `bcs(VersionedDkgConfirmation::V1(dkg_v1::Confirmation))` | `RandomnessDkgConfirmation(authority)` |
| `RandomnessStateUpdate(u64, Vec<u8>)` | 5 | deprecated | rejected by voting (CV:119-123) |

- Built by `new_randomness_dkg_message` / `_confirmation` (MC:694-717); the versioned types are at
  MC:547-556, and `is_valid_version` accepts only `(V1, 1)` (MC:597-599).
- Block voting rejects payloads over `dkg_v1::DKG_MESSAGES_MAX_SIZE` (CV:99-110).
- After commit, `verify_consensus_transaction` drops one whose `authority` is not the block author
  (EPS:3075-3097).
- Dedup is per authority for the whole epoch (CH:2930-2990): the first sequenced message (and the
  first confirmation) per authority wins. A validator restarted before merging submits a fresh
  message (RND:564-622); the dedup drops it.
- Processed even after certificates close (CH:2726-2731), but dropped once the initial reconfig
  state is `RejectAllTx` (CH:2735-2740).
- `build_commit_handler_input` collects `(authority, bytes)` in commit order (CH:3088-3102).

### R3.4 [E] Commit handler: `process_dkg_updates` (CH:2206-2259)

Called once per commit after `init_randomness` (R4.1), so the commit that completes DKG does not
itself reserve a round:

1. If `!randomness_state_enabled()`, DKG traffic is a `debug_fatal` and ignored (CH:2214-2225).
2. Each message: `bcs::from_bytes`; on failure warn and skip; else `add_message`
   (CH:2261-2291).
3. Each confirmation: decode, else skip; `add_confirmation` (CH:2293-2324).
4. `advance_dkg(output, commit_info.round)` (`round` = the commit's leader round) if any message or
   confirmation decoded, or if `always_advance_dkg_to_resolution` (or mainnet epoch ≥ 1143) and DKG
   is still pending (CH:2242-2257).

`add_message` (RND:829-877) ignores the message if: the version is not valid; `used_messages` or
`dkg_output` is already set; the authority is unknown (`debug_fatal`); `msg.sender != party_id`
(equivocation); or a message from that sender was already enqueued or processed. Otherwise it
spawns `role.process_message(msg)` on a blocking thread.

`add_confirmation` (RND:880-917) ignores the confirmation if the version is not valid, DKG output
is already set, the authority is unknown, or the sender does not match. Otherwise it inserts it
(BTreeMap by PartyId, so a later one from the same sender replaces it; dedup makes that moot) and
writes it to the commit output. Confirmations are accepted **before** the local merge.

### R3.5 [E] `advance_dkg` (RND:628-659)

1. **`try_merge_messages`** (RND:662-740), unless `dkg_output` or `used_messages` is set: awaits
   every enqueued processing task. Each `Ok(Some(processed))` goes into `processed_messages`
   (BTreeMap by PartyId) and the commit output. Then `role.merge(processed_messages.values())`, in
   PartyId order:
   - Party (TBLS:521-575): dedup by sender; `NotEnoughInputs` if `Σ weight < t'`. Otherwise the
     used set is **all** processed messages; the confirmation lists this party's complaints. If
     every used message carries a complaint by this party, it is a `GeneralError` (local; R3.9).
   - Observer (TBLS:1077-1110): the same weight check, without complaints.
   - On success: set `used_messages` and write it to the output. A Party submits its
     `RandomnessDkgConfirmation` through consensus. `NotEnoughInputs` waits; other errors are
     logged.
2. **`try_complete_dkg`** (RND:743-826), if `used_messages` is set and `dkg_output` is not:
   `role.complete(used_messages, confirmations)`:
   - `process_confirmations` (TBLS:582-704; Observer TBLS:1120-1240): drop confirmations from
     unknown or zero-weight senders, dedup by sender; `NotEnoughInputs` unless `Σ weight ≥ 2t'−1`.
     For each confirmation's complaints, in order: if `check_complaint_proof` verifies (the accused
     message is in the used set, and the recovery package proves bad shares to the accuser), the
     accused is excluded; otherwise the accuser is excluded and the rest of its complaints are
     skipped. Error if no message survives.
   - `aggregate` (TBLS:707-776; Observer TBLS:1242-1257): `vss_pk = Σ vss_pk` of the surviving used
     messages. A Party also sums its shares; `shares = None` if it complained about any surviving
     message or has weight 0.
   - On success: `dkg_output = Some(output)` (also to the output). Set the receiver's public key to
     `vss_pk.c0()` (R5.4). A Party calls `network_handle.update_epoch(...)` (R5.2).
3. **Timeout**: if `dkg_output` is still unset and `round > random_beacon_dkg_timeout_round`, set
   `dkg_output = None` (failed) and write it to the output (RND:638-656).

`dkg_status()` (RND:960-966): `Pending` while unset, `Successful` for `Some(Some)`, `Failed` for
`Some(None)`.

### R3.6 [E/I] When `advance_dkg` runs

At 138 it runs on every commit while DKG is pending (R3.4.4), so a failure is declared at the first
commit whose leader round exceeds 3000. Before v127 it ran only on commits carrying DKG traffic, so
the timeout fired at the first such commit after round 3000. Both rules are functions of consensus
output.

### R3.7 [E] Message validity (what every party agrees on)

`Party::process_message` (TBLS:383-474) returns `Err` (the message is dropped) **only** from
`Message::sanity_check` (TBLS:102-163), which uses public data: a known sender of nonzero weight,
polynomial degree `t'−1`, `c0 ≠ 0`, `encrypted_shares.len() == num_nodes`, and the ECIES
multi-recipient NIZK verifies under `random_oracle.extend("encs {sender}")`. A share that fails to
decrypt or verify for this party yields `Ok` with a complaint, so the message still counts.
`Observer::process_message` (TBLS:1059-1066) runs the same `sanity_check`. So every honest party and
observer processes the same message set, and merges it at the same commit.

### R3.8 [E/I] Stored state (epoch store; per epoch)

Tables (EPS:519-550): `dkg_processed_messages_v2: PartyId → VersionedProcessedMessage` (includes
this party's **decrypted shares**), `dkg_used_messages_v2`, `dkg_confirmations_v2`,
`dkg_output_v2: Option<Output>` (`None` = failed), `randomness_next_round`,
`randomness_highest_completed_round`, `randomness_last_round_timestamp`,
`congestion_control_randomness_object_debts`. Except `randomness_highest_completed_round` (written
directly by the reporter, RND:1042-1058), all are written through `ConsensusCommitOutput`
(CQ:77-90, CQ:222-246), so they reach the DB with the quarantined commit output (CQ:344-366).
`get_randomness_last_round_timestamp` reads quarantine first, then the DB (EPS:2784-2797,
CQ:911-918).

### R3.9 [E] What is deterministic, what is local

| Item | Function of consensus output? |
|---|---|
| Set of processed messages | Yes (R3.7) |
| Commit at which the merge happens; `used_messages` | Yes, for every observer and every honest party. Exception: a Party whose used messages **all** carry its own complaints fails the merge locally (TBLS:565-570) and stays pending (uncertain whether reachable without a Byzantine supermajority of dealers) |
| Commit at which DKG completes | Yes: the first commit after which the merge happened and accepted confirmations carry ≥ 2t'−1, with complaint checks on public data only |
| Commit at which DKG fails | Yes: leader round > 3000 with no output (R3.6) |
| `vss_pk` (whole polynomial) and `c0` | Yes; no private key needed (Observer `aggregate`, TBLS:1242-1257). The reference's `test_dkg_observer` asserts the observer's `vss_pk` equals the validators' (RND:1471-1505) |
| Own shares | Local (needs the protocol secret key) |
| DKG message and confirmation bytes a party submits | Local (random polynomial; random complaint proofs) |

**(a)** Yes: a validator with no shares (an `Observer`, or a party that sends nothing) can compute
the commit at which DKG completes or fails, exactly as parties do. It must run the same
`sanity_check`, merge and `process_confirmations` logic over the sequenced messages, with the same
`Nodes` (R3.2) and random oracle.

**(b)** Yes: `vss_pk = Σ` of the surviving messages' public commitments; `c0` verifies full
signatures and the full polynomial verifies partial signatures (`partial_verify_batch`). Deriving
it needs no private key. Only shares need the protocol secret key: decrypting one's own
`encrypted_shares` entry.

---

## 4. Randomness rounds in the commit handler

### R4.1 [E] Reserving a round: `init_randomness` (CH:966-1014)

Runs before the commit's DKG updates and transactions (CH:1137). If randomness is enabled:
- `Pending`: no round; `dkg_failed = false`.
- `Failed`: no round; `dkg_failed = true`.
- `Successful`: if the commit's **initial** reconfig state `should_accept_tx()` (not yet
  `RejectAllTx`), `reserve_next_randomness(commit_info.timestamp)` (RND:921-950): no round if a
  previous round exists and `timestamp − last_round_timestamp < 500`. Otherwise
  `round = next_randomness_round`; the output records `(round + 1, timestamp)` (CQ:222-229).

`commit_info.timestamp` is the commit timestamp, raised to the epoch start time if below it
(CH:266-276). The first commit after DKG completes always reserves round 0 (no previous round in
the epoch).

### R4.2 [E] Round numbering

`next_randomness_round` lives in the per-epoch store and defaults to 0 (RND:532-536), so rounds
restart at 0 each epoch. `random::update_randomness_state` accepts either the very first update
with round 0, `epoch > stored epoch` with round 0, or `round = stored + 1` (RMOVE:88-118). Skipped
epochs (DKG failed) are allowed.

### R4.3 [E] Splitting and ordering (`merge_and_reorder_transactions`, CH:1965-2009)

Previously deferred transactions are loaded (CH:2011-2069). This round's `ConsensusRound` keys come
first. Then, **only if** `dkg_failed || randomness_round.is_some()`, every `Randomness`-keyed
deferral is loaded (`full_range_for_randomness`, CH:2041-2061, EPS:2281-2286). Transactions are split by
`uses_randomness()` (any shared input is `0x8`, TX:3008-3012); congestion-deferred randomness
transactions move back to the randomness list. Each list is reordered by
`consensus_transaction_ordering` (gas price).

### R4.4 [E] Deferral: `should_defer` (EPS:2368-2405)

First: if `!dkg_failed && !generating_randomness && randomness_state_enabled() &&
uses_randomness()`, defer under `DeferralKey::Randomness { deferred_from_round }` (the original
round, kept across re-deferrals) with reason `RandomnessNotReady`. Such deferrals have no limit
(TD:84-100) and skip congestion cancellation (CH:1911-1916). `DeferralKey` sorts all
`Randomness` keys before `ConsensusRound` keys (TD:8-21). Otherwise the congestion rules apply
(`congestion_control_reference.md`).

Consequences: while DKG is pending, every randomness-using transaction is deferred. In a commit
that skips a round (interval < 500 ms), the newly committed ones are deferred and the stored ones
stay stored.

### R4.5 [E] DkgFailed cancellation (CH:1439-1450)

With `dkg_failed`, every randomness-using transaction (loaded or new) is cancelled with
`CancelConsensusCertificateReason::DkgFailed` before any other check, and scheduled. Version
assignment gives `0x8` the marker `SequenceNumber::RANDOMNESS_UNAVAILABLE` and other shared inputs
`CANCELLED_READ` (SOVM:518-523). Execution reports `ExecutionCancelledDueToRandomnessUnavailable`.
The prologue lists these cancellations (`include_cancelled_randomness_txns_in_prologue`).

### R4.6 [E] The randomness congestion tracker

A separate tracker is built for the randomness list (`init_congestion_tracker(.., true, ..)`,
CH:1405-1406), with budget and burst scaled by `randomness_scalar` (20%), its own debts
(`congestion_control_randomness_object_debts`, decayed by
`max_accumulated_randomness_txn_cost_per_object_in_mysticeti_commit`). Randomness transactions
that pass R4.4 go through the same `handle_deferral_and_cancellation` (CH:1452-1462), so they can be
congestion-deferred (`ConsensusRound` keys) or cancelled. Its debts are stored as the regular
ones are (CH:1495-1526). Details: `congestion_control_reference.md`.

### R4.7 [E] Schedulables (CH:1207-1236, CH:1408-1410)

- Regular: `[ConsensusCommitPrologue, AuthenticatorStateUpdate?, regular txns…]`.
- Randomness: `[RandomnessStateUpdate(epoch, round)?, randomness txns (scheduled or DkgFailed/
  congestion cancelled)…]`, the update first when a round was reserved.

---

## 5. Generating the bytes, and the checkpoint

### R5.1 [E/I] Starting a round

After the commit output is pushed, `generate_randomness(epoch, round)` (CH:1268-1273,
RND:953-958) tells the network loop (Party only) `send_partial_signatures(epoch, round)`. That sets
`highest_requested_round` and starts send tasks (NET:393-420). Restart re-requests every reserved
round above the highest completed one (RND:542-558).

### R5.2 [I] The network event loop (`RandomnessEventLoop`, NET:210-1087)

- `update_epoch` (NET:318-390), called by a Party when its DKG completes (or at load): it records
  `authority_info`, each peer's share ids (`dkg_output.nodes.share_ids_of(party)`), `vss_pk`,
  `aggregation_threshold = t'`, and the allowed-peer set (the committee's PeerIds). It aborts old
  send tasks and drains buffered next-epoch partial signatures.
- `maybe_start_pending_tasks` (NET:833-918): needs `dkg_output.shares`, so **a party without shares
  sends nothing**, but it still receives and aggregates. For each round from the highest
  completed + 1 to the highest requested (≤ 20 concurrent, P2P:638), it computes
  `partial_sign_batch(shares, round.signature_message())`, records its own partials, and spawns
  `send_signatures_task`.
- `send_signatures_task` (NET:939-1009): forever, every 5 s (P2P:644), sends
  `SendSignaturesRequest` to every committee member but itself, each request with a 10 s
  timeout, until the task is aborted. Each request carries the partials, or once the full signature
  is known, `partial_sigs: []` and `sig: Some(full)`. Tasks abort on `complete_round` (R5.5) or
  `update_epoch`.
- `receive_partial_signatures` (NET:455-571): drops a request that is for an old epoch, more than
  one epoch ahead, already completed, from an unknown peer, carries the wrong share count or ids,
  or is ≥ 50 rounds (P2P:631) past the last completed round. Future-epoch partials are buffered.
  The rest are stored, then `maybe_aggregate_partial_signatures`.
- `maybe_aggregate_partial_signatures` (NET:574-695): waits until the round is locally requested
  (local consensus reached it), then `ThresholdBls12381MinSig::aggregate(t', partials)`, verified
  against `vss_pk.c0()`. On failure each peer's partials are batch-verified against `vss_pk`, bad
  peers dropped (and maybe blocked, up to 20% of the weight, NET:795-831), and it retries.
- `receive_full_signature` (NET:698-753): if the round is locally requested and not completed,
  verify against `c0`. Otherwise the signature is **dropped, not buffered**.
- `process_valid_full_signature` (NET:755-793): sets the send task's full-sig cell, records the
  signature, and sends `(epoch, round, bcs(sig))` to `RandomnessRoundReceiver`.

### R5.3 [E] Wire protocol

anemo RPC service `Randomness`, package `sui`, method `send_signatures`, route `SendSignatures`,
BCS codec `mysten_network::codec::BcsCodec` (`crates/sui-network/build.rs:21`, `:226-238`).
Request (NET:52-63): `SendSignaturesRequest { epoch: u64, round: RandomnessRound(u64),
partial_sigs: Vec<Vec<u8>> (each bcs(PartialSignature<G1>), ordered by share index), sig:
Option<G1Element> }`; response `()`. The server (`crates/sui-network/src/randomness/server.rs`)
forwards to the loop; the router allows only peers in the allowed set (the committee, once DKG
completed; empty before), returning 404 otherwise (`auth.rs`, NETB:79-82), with an inflight limit
of 20 (P2P:659). There is **no pull RPC**: a node gets signatures only when peers push them.

### R5.4 [E] Signature and random bytes

- Scheme: `ThresholdBls12381MinSig` (signatures in G1, keys in G2). `RandomnessSignature =
  fastcrypto_tbls::types::Signature` (CRY:1809-1810).
- Message: `b"random_beacon round " || bcs(round.0 as u64)` (CRY:1861-1868).
- The full signature is `sk·H(m)` for the group secret, **unique** whichever t'-weight of shares
  is aggregated.
- `random_bytes = bcs::to_bytes(&sig)` (NET:780): the 48-byte compressed G1 point, with no length
  prefix (fastcrypto's fixed-array serde) (uncertain about the exact length encoding; byte-compare
  with the reference).

### R5.5 [E] The transaction (RRR:247-345)

`handle_new_randomness(epoch, round, bytes)`: drops it if the epoch is not current. Broadcasts to
consensus-observer subscribers (R6.4). Builds `VerifiedTransaction::new_randomness_state_update(epoch,
round, bytes, randomness_obj_initial_shared_version)` (TX:4511-4524): a system transaction (sender
`0x0`, no gas, empty signature) whose kind is `TransactionKind::RandomnessStateUpdate(
RandomnessStateUpdate { epoch, randomness_round, random_bytes, randomness_obj_initial_shared_version })`
(TX:467-476). Its only input is `0x8`, mutable, at that initial version (TX:2066-2072). It is
persisted at once (`persist_transaction`, RRR:297), because peers stop sending once the round is
certified. Then `insert_tx_key(TransactionKey::RandomnessRound(epoch, round), digest)` (RRR:300)
unblocks the checkpoint builder (TX:5619-5623). Execution runs
`0x2::random::update_randomness_state(Random, round: u64, bytes: vector<u8>)` (EXE:1821-1824,
EXE:2338-2370; RMOVE:88-118); a failure is fatal (RRR:330-335).

When a certified checkpoint containing the round executes, `notify_randomness_in_checkpoint`
(CKE:474-493, RND:1042-1058) records the highest completed round and calls `complete_round`, which
stops the sends. **[I]**

### R5.6 [E] Placement (`create_pending_checkpoints`, CH:1536-1700)

1. `should_write_random_checkpoint = randomness_round.is_some() || (dkg_failed &&
   !randomness_schedulables.is_empty())` (CH:1551-1552).
2. Regular schedulables are chunked by `max_transactions_per_checkpoint` (20,000), each chunk
   taking `next_height()` and, with accumulators on (they are at 138), its own
   `AccumulatorSettlement(epoch, height)`. **Then** the randomness schedulables, if written, are
   chunked the same way, taking the next heights (CH:1590-1594). With no round and no DkgFailed
   cancellations, randomness transactions take no height (they are all deferred).
3. Shared versions are assigned over regular then randomness schedulables in one pass (EPS:3137-3165),
   so the update takes `0x8`'s next version before the randomness transactions that read it.
4. Regular chunks are pushed into the `CheckpointQueue`, then `flush(timestamp, final_round)`
   (flushes if `timestamp ≥ last_built + min_checkpoint_interval_ms`, or forced). **Then** the
   randomness chunks are pushed (CH:1662-1681): they wait in the queue and become the **first**
   roots of the next flushed pending checkpoint, ahead of the following commits' roots. On the
   final commit they are flushed at once (`flush(.., true)`); the last pending checkpoint is marked
   `last_of_epoch` (CH:1683-1685). A `push_chunk` that would exceed `max_tx` forces a flush first
   (CH:609-670). A flushed pending checkpoint takes the last root's timestamp, height and commit ref
   (CH:684-716).
5. The commit's recorded height is the last randomness chunk's height, else the last regular
   chunk's (CH:1645-1649).
6. The checkpoint builder resolves each root chunk separately: it waits for the keys' digests
   (`notify_read_tx_key_to_digest`, CKP:1625, which blocks on the update until the signature
   exists), causally sorts the chunk, and appends its settlement (CKP:1591-1660). So the update is
   in its own chunk, ordered before the randomness transactions that read it.

### R5.7 [E] `randomness_rounds` in the summary

The builder collects `digest → round` for every `RandomnessStateUpdate` among the checkpoint's
transactions (CKP:1937, CKP:1972-1975), and lists them in the order of the checkpoint's effects
(CKP:2147-2151). `CheckpointSummary::new` writes `bcs(CheckpointVersionSpecificData::V1 {
randomness_rounds })` when `checkpoint_summary_version_specific_data == 1` (MCK:370-395,
MCK:1222-1225). Anchovy computes it the same way (`anchovy/crates/checkpoint/src/contents.rs:92-95`,
`anchovy/crates/checkpoint/src/summary.rs:141-160`).

### R5.8 [E] Is an update fully determined?

Yes. Each part is fixed as follows:
- Whether a commit has a round, and its number: by consensus output (R3.9, R4.1).
- `epoch` and the initial shared version: by the epoch start (R2.2).
- `random_bytes`: the BCS of the unique threshold signature on a message fixed by the round,
  under the `vss_pk.c0()` fixed by consensus (R3.9b).
- The transaction digest: by these four fields.
- Its chunk, height, pending checkpoint, order, shared versions and settlement: by the commit
  handler's deterministic queue (R5.6).

Only **when** a validator can execute it is local: until it has the signature, its checkpoint
builder blocks at that pending checkpoint and every later one. **[E]** for liveness: a validator
that never obtains the signature for some round cannot build checkpoints past it, and can only
continue by syncing certified checkpoints.

---

## 6. Restart and epoch boundaries

### R6.1 [E/I] Reference restart

`RandomnessManager::try_new` (RND:329-561) rebuilds `Nodes`/role from the epoch start, then loads
from the DB (flushed quarantine output only):
- `dkg_output_v2`: `Some(Some)` restores the output, the receiver key and (Party) `update_epoch`
  with the recovered highest completed round. `Some(None)` restores Failed.
- Otherwise it loads processed messages, used messages and confirmations.
- `next_randomness_round`.
- It re-requests incomplete reserved rounds.

Consensus then replays commits after the last flushed one, and the commit handler re-derives the
rest; R3.9 makes this replay identical. `start_dkg` resubmits a fresh message only if DKG had not
merged (dedup keeps the first, R3.3). Persisted update transactions re-execute without the
signature (R5.5).

### R6.2 [E] Epoch start

DKG runs at each epoch's start, for that epoch only (NODE:1580-1602). There is no next-epoch DKG
ahead of time. The receiver's key is cleared at epoch start (NODE:1580-1582). Until DKG completes,
the epoch has no randomness: randomness-using transactions are deferred (R4.4).

### R6.3 [E] Epoch end

- Rounds are reserved while the commit's initial reconfig state is not `RejectAllTx`
  (R4.1), so rounds continue while the epoch close drains deferrals (`RejectAllCerts`), and the
  final commit (the one that moves to `RejectAllTx`) still reserves one. Its update lands in the
  epoch's last checkpoint (R5.6.4).
- Randomness-deferred transactions block a normal close (`deferred_transactions_empty`) until
  `epoch_close_deadline_ms` (120 s), then are abandoned (`congestion_control_reference.md` R2.7).
- With DKG pending all epoch, randomness transactions are deferred all epoch and only the
  deadline closes it **(uncertain: no test observed)**.
- DKG traffic after `RejectAllTx` is dropped (R3.3).

### R6.4 [I] Consensus observer relay

Validators that run a consensus observer server (off unless `observer.server_port` is set,
`consensus/config/src/parameters.rs:381-387`) merge verified randomness signatures into the
observer block stream (`consensus/core/src/observer_service.rs:301-311`). Observers feed them to
`handle_consensus_randomness_signature` (RRR:189-245), which verifies them against `c0` (buffering
until DKG completes) and builds the same update. This is how non-committee nodes get signatures.

---

## 7. Test networks and the compat harness

### R7.1 Sui test networks

`ConfigBuilder`/sui-swarm networks have a chain identifier that is neither mainnet nor testnet:
Chain::Unknown. At 138 `random_beacon` is on (R1.1), genesis creates `0x8` (R2.1), and every sui
validator runs a `RandomnessManager` Party and the p2p `Randomness` service (NODE:1271-1281,
NODE:1366-1370).

### R7.2 The compat harness (`anchovy/crates/compat/tests/one_of_each.rs`)

- Committee: stakes `[3S, 3S, S]`, capped to voting power 3334/3334/3332; anchovy is the third
  (`:244-299`). No override touches randomness: the overrides change the gas model
  (`:1071-1080`) and congestion (`:1149-…`). The harness does not turn randomness off.
- It avoids randomness only implicitly:
  1. No test submits a transaction with `0x8` as input.
  2. **DKG cannot complete in this committee without anchovy's confirmation**: the sui
     validators' reduced weight is 350 < 2t'−1 = 351 (R3.2). So sui stays `Pending`, no round is
     ever reserved, and no `RandomnessStateUpdate` exists. After leader round 3000, DKG becomes
     `Failed`, which changes nothing while no randomness transaction exists (R5.6.1). Default
     epochs last 24 h (`crates/sui-config/src/genesis.rs:428-431`) unless a test sets them.
- If the sui side alone could complete DKG (e.g. three sui validators with weight ≥ 351 reduced,
  or a smaller anchovy share), sui would reserve round 0 at the next commit, and anchovy would fork
  at that commit's checkpoints: anchovy would lack the randomness chunk's height, the update, and
  its settlement.

---

## 8. Gaps in anchovy

Today (decode `anchovy/crates/validator/src/consensus/decode.rs`, commit `commit.rs`, sequence
`sequence.rs`, deferral `deferral.rs`, vote `vote.rs`, checkpoint `anchovy/crates/checkpoint`):

1. **Nothing tracks DKG.** Voting checks only DKG payload size and rejects the deprecated update,
   as the reference does (`vote.rs:396-407`). The commit decoder maps DKG messages and confirmations
   to `Decoded::Other` (`decode.rs:363`): there is no author check, no per-authority dedup, no
   processing, no merge or confirmation weight, no timeout, no `dkg_status`.
2. **Randomness-using transactions are always admitted** in the commit's regular chunk, last,
   skipping congestion control (`commit.rs:389-392`, `sequence.rs:150-178`, `commit.rs:757-758`).
   The reference instead defers them under `Randomness` keys while pending or between rounds
   (R4.4), cancels them with `RANDOMNESS_UNAVAILABLE` once DKG fails (R4.5), runs a separate
   randomness congestion tracker with its own debts (R4.6), and schedules them in separate
   randomness chunks queued after the flush (R5.6). Any committed randomness transaction forks
   anchovy today, whatever the DKG state.
3. **No randomness rounds**: no `reserve_next_randomness`, `randomness_next_round` or
   last-round timestamp; no `RandomnessStateUpdate` schedulable; no randomness chunk height or
   settlement. This forks at the first commit after sui's DKG succeeds (R7.2).
4. **No `Randomness` deferral keys are produced or loaded.** `DeferralKey::Randomness` exists
   (`deferral.rs:17-18`, encoded with tag 0), but the "load all randomness deferrals iff round or
   DKG failed" rule is missing (R4.3). Epoch close does not wait on them; the design doc already
   lists close draining as missing.
5. **No prologue entries for DkgFailed cancellations.** The executor already maps
   `RANDOMNESS_UNAVAILABLE` to `ExecutionCancelledDueToRandomnessUnavailable`
   (`anchovy/crates/executor/src/execution_params.rs:113-114`) and executes
   `RandomnessStateUpdate` (`anchovy/crates/executor/src/execution_engine.rs:981`).
6. **No signature source**: no anemo endpoint, network identity at the p2p address, or
   `Randomness` service. Anchovy never receives pushed partial or full signatures. It also has
   no way to build the update, insert the round's key, or persist the bytes.
7. **Restart**: anchovy replays the epoch from its start. Rounds certified before a crash need
   their `random_bytes` from local storage (anchovy's own checkpoint contents or a dedicated table),
   because peers stop sending completed rounds (R5.5).
8. **No liveness fallback** when a signature never arrives (R5.8): anchovy has no state sync of
   certified checkpoints from peers.
9. **Already matching**: `RandomnessStateCreate` at epoch end (`close.rs:189-190`),
   `randomness_rounds` in the summary (R5.7), DKG/update message decoding
   (`anchovy/crates/messages/src/consensus.rs:470-478`).

---

## 9. Options for a non-participating validator

### Option A: observe DKG, fetch signatures (no shares)

- **DKG.** Run `dkg_v1::Observer` (fastcrypto-tbls, already a sui dependency) over the sequenced
  messages and confirmations, with the reference's `Nodes::new_reduced`, `t`, random oracle,
  per-authority dedup, author check, version check, BTreeMap-by-PartyId ordering, and commit
  timing (R3.3–R3.7). This gives the completion or failure commit and `vss_pk`, deterministically
  and without secrets (R3.9). On replay from epoch start it recomputes them with no persisted
  state.
- **Rounds and scheduling.** R4.1–R4.7 and R5.6 are pure commit-handler logic.
- **Bytes.** Accept `SendSignaturesRequest` pushes from sui validators, which keep sending to every
  committee member (anchovy included) every 5 s until the round is certified (R5.2). Aggregate
  t'-weight of partials (verified against the public `vss_pk`), or verify a full `sig` against
  `c0`. Buffer early arrivals (unlike the reference, which drops full signatures for rounds its
  consensus has not reached). Persist the bytes per `(epoch, round)`.
- **Cost.** No secret handling, no consensus submissions, no outbound RPCs. Anchovy's protocol key
  stays unused for DKG.
- **Risks.**
  - **Liveness of DKG**: anchovy's stake neither deals nor confirms. DKG completes only if the
    others hold ≥ t' reduced weight in messages and ≥ 2t'−1 in confirmations. In the compat
    committee they do not (350 < 351), so DKG always fails, randomness is disabled every epoch, and
    randomness transactions are cancelled. That is consistent, not a fork. On a larger network,
    non-participating stake above roughly a third of the weight fails DKG.
  - **Signature liveness**: anchovy depends on pushes. It misses a round if it is unreachable while
    sui certifies that round, since there is no pull RPC (R5.3). The fallbacks are syncing the
    certified checkpoint (the update transaction is in its contents) or the consensus observer
    stream (off by default, R6.4).
  - Rounds complete only if sui alone holds ≥ t' reduced weight with shares.
- **Precedent.** A reference validator whose DKG output has `shares = None` behaves this way: it
  sends nothing, but still sets `highest_requested_round` and aggregates or accepts what peers send
  (NET:393-420, NET:833-843).

### Option B: participate fully

- **DKG.** At epoch start, create a `dkg_v1::Party` from the protocol secret key (R3.1) and submit
  `RandomnessDkgMessage`. Process messages (decrypting own shares). On merge, submit
  `RandomnessDkgConfirmation`. Complete to get shares. All submissions go through consensus,
  author-signed blocks as today.
- **Rounds.** Partial-sign each reserved round. Push `SendSignaturesRequest`s to every peer until
  the round is in a certified checkpoint, switching to the full `sig` once known. Receive peers'
  pushes and aggregate.
- **Restart.** Replay from epoch start reproduces the output and shares (processing is a
  deterministic decryption). It needs only the protocol key, not the persisted processed messages.
  Its own message's secret is not needed (own shares come from decrypting own entry). If its
  message was already sequenced, a resubmitted one is deduplicated.
- **Benefit.** Anchovy's stake counts toward DKG and signature thresholds. Required for the compat
  committee to have randomness at all, and for networks where anchovy holds over about a third of
  the stake.
- **Cost.** Secret handling (shares in memory; the reference persists them in
  `dkg_processed_messages_v2`), consensus submissions with retries, outbound anemo client, peer
  blocking rules.

### Network-level requirements (either option)

- An anemo (QUIC/TLS) endpoint listening on the validator's **p2p address from the system state**.
  Sui validators dial committee peers at chain-sourced addresses (`update_peer_addresses`,
  NODE:2819-2849), so anchovy need not run discovery to be reached.
- TLS identity = the validator's Ed25519 **network key**; PeerId = its public key bytes, which
  must equal `narwhal_network_pubkey` in the system state (R3.1).
- `server_name = "sui-{chain_identifier}"` (NODE:1366-1370). Frame limits as sui's (request 1 MiB,
  response 128 MiB, NODE:1322-1325).
- Route `sui.Randomness/SendSignatures` with BCS `SendSignaturesRequest` → `()` (R5.3).
- Sui peers also issue discovery and state-sync RPCs on the same connection. Anchovy can answer
  them as unimplemented, at some cost in log noise and peer scoring **(uncertain how sui's
  state sync and discovery treat such a peer)**.
- Option B also needs: dialing peers at their system-state p2p addresses. Peers accept anchovy's
  pushes only after their own DKG completes (their allowlist is empty before, R5.3), so anchovy
  must retry as sui does.
