# Sui Protocol Upgrades: Behavioral Reference

This document specifies how the reference Sui validator agrees on, and moves to, a new protocol
version (and new system packages) at an epoch boundary. It is written so that anchovy can
reproduce the required behavior in a network mixed with sui validators, and tell which parts are
artifacts of the reference's structure.

Requirements are numbered (R<section>.<n>) and tagged as in `reconfiguration_reference.md`:

- **[E] Essential.** Needed for agreement with other validators (no fork), safety or liveness.
- **[I] Incidental.** How the reference happens to do it.
- **[E/I]** The property is essential, the mechanism incidental.

Uncertain statements are marked **(uncertain)**. Line numbers are relative to the repo root at
commit `5fa13b9ed3`.

| Abbrev | File |
|---|---|
| NODE | `crates/sui-node/src/lib.rs` |
| AUTH | `crates/sui-core/src/authority.rs` |
| EPS | `crates/sui-core/src/authority/authority_per_epoch_store.rs` |
| CH | `crates/sui-core/src/consensus_handler.rs` |
| CV | `crates/sui-core/src/consensus_validator.rs` |
| MC | `crates/sui-types/src/messages_consensus.rs` |
| SPV | `crates/sui-types/src/supported_protocol_versions.rs` |
| PC | `crates/sui-protocol-config/src/lib.rs` |
| TX | `crates/sui-types/src/transaction.rs` |
| FW | `crates/sui-framework/src/lib.rs` |
| EXE | `sui-execution/latest/sui-adapter/src/execution_engine.rs` |
| SWC | `crates/sui-swarm-config/src/network_config_builder.rs` |

---

## 0. Overview

1. At the start of every epoch (and so at node start), each validator submits an
   `AuthorityCapabilitiesV2` through consensus: the protocol versions its binary supports, each
   with the digest of that version's `ProtocolConfig`, and the object references of the system
   packages its binary carries.
2. The commit handler keeps, per authority, the capabilities with the highest generation, until
   the epoch's certificates close. Later notifications are dropped by every validator alike.
3. When the checkpoint builder builds the epoch's last checkpoint, it picks the highest version
   `current + 1, current + 2, ...` that enough stake supports with the same config digest and the
   same system packages. It writes that version, and the bytes of any system package that
   changes, into the `ChangeEpoch` at the end of the epoch's last transaction.
4. Executing `ChangeEpoch` writes the version into the system state, and publishes or upgrades
   the system packages. The next epoch's `ProtocolConfig` is read from the system state.
5. A node whose binary does not support the new version exits.

Nothing else in an epoch depends on the next version: consensus, voting and the commit handler
all use the epoch's own `ProtocolConfig`, fixed at its start (§5).

---

## 1. Capabilities

### R1.1 [E] The message: `ConsensusTransactionKind::CapabilityNotificationV2`

`AuthorityCapabilitiesV2` (MC:404-436) has:

- `authority: AuthorityName`, the sender, which must equal the block's author (R1.5);
- `generation: u64`, set to the wall-clock milliseconds when made (`AuthorityCapabilitiesV2::new`,
  MC:439-461). Only its order matters: the highest wins (R1.6);
- `supported_protocol_versions: SupportedProtocolVersionsWithHashes`, a list of
  `(ProtocolVersion, Digest)`, one per supported version (SPV:57-60);
- `available_system_packages: Vec<ObjectRef>`, the references of the system packages the binary
  carries, as they would be on chain (R1.3).

It travels as `ConsensusTransactionKind::CapabilityNotificationV2` (variant index 8, MC:511),
built by `ConsensusTransaction::new_capability_notification_v2` (MC:669-677), whose tracking id is
a hash of the capabilities. Its dedup key is `ConsensusTransactionKey::CapabilityNotification(
authority, generation)` (MC:255).

V1 (`CapabilityNotification`, `AuthorityCapabilitiesV1`, without digests) is deprecated. Block
verification rejects it (CV:112-117), and the commit handler treats one as a bug and skips it
(CH:2742-2752). The feature flag `authority_capabilities_v2` is on from protocol 96 on every
chain (PC:4340), and `get_capabilities_v2` asserts it (EPS:2714).

### R1.2 [E] The digest of a version

`SupportedProtocolVersionsWithHashes::from_supported_versions` (SPV:81-97) lists every version in
`min..=max`, each with `protocol_config_digest(ProtocolConfig::get_for_version(v, chain))` (SPV:73-79):
Blake2b-256 (`DefaultHash`) over the BCS of the whole `ProtocolConfig`, for the chain of the
network's chain identifier (`get_chain_identifier().chain()`, NODE:2002).

The digest is what makes two binaries' "version N" the same version: validators vote for a
`(version, digest)` pair (R2.2). So a reimplementation must produce the reference's
`ProtocolConfig` byte for byte, struct field order and all, for every version it advertises.

### R1.3 [E] Available system packages

`AuthorityState::get_available_system_packages` (AUTH:5380-5419) goes through
`BuiltInFramework::iter_system_packages` and, for each, calls `sui_framework::compare_system_package`
(FW:198-…) with the binary's modules, dependencies and the epoch's binary config:

- a package not on chain yields the reference of a fresh package at `OBJECT_START_VERSION`, built
  with the genesis marker as its previous transaction (FW:208-223);
- a package whose on-chain modules equal the binary's yields the current on-chain reference;
- a compatible upgrade yields the reference the upgraded package would have (next version, built
  on the current version's previous transaction);
- an incompatible one makes the whole function return an empty list (AUTH:5413-5415), which is a
  vote against any change (R2.2).

The list is sorted before comparing (AUTH:5562). [E] for the outcome: every validator with the
same binary framework gets the same list.

### R1.4 [E/I] Who sends them, and when

`SuiNode::monitor_reconfiguration` (NODE:1924-…) submits capabilities at the top of each loop
iteration, that is when the node starts and after every reconfiguration (NODE:1964-2014), if the
node has validator components and is in the epoch's committee (fullnodes running consensus do
not). The supported range is the configured `supported_protocol_versions`
(`SupportedProtocolVersions::SYSTEM_DEFAULT`, `MIN..=MAX`, unless a test sets it: NODE:492-498),
truncated below the current version (`truncate_below`, SPV:48-51). Its maximum is lowered past
any version that enables accumulators while the accumulator root does not exist (NODE:1980-1995).

Each submission is a fresh generation, so a restart mid-epoch sends another that supersedes the
first. **[I]** that it is resent on restart: the epoch store keeps the recorded set (R1.6).

### R1.5 [E] Voting and the author check

Block verification accepts `CapabilityNotificationV2` without inspecting it (CV:126-128).
After commit, `verify_consensus_transaction` drops one whose `authority` is not the block's
author (EPS:3027-3049): sequenced transactions with a mismatched sender are ignored.

### R1.6 [E] Recording: last generation per authority, until certificates close

The commit handler collects the commit's notifications (CH:3104-3109), and
`process_capability_notifications` (CH:2136-2145) calls `record_capabilities_v2` (EPS:2692-2711)
for each, in commit order: it keeps the stored one if its generation is greater than or equal,
otherwise replaces it, in the table `authority_capabilities_v2: DBMap<AuthorityName,
AuthorityCapabilitiesV2>` (EPS:500).

Once the epoch's certificates have closed (`!should_accept_consensus_certs()`, decided by earlier
commits, so alike everywhere), capability notifications are dropped (CH:2690-2723). The set the
epoch's last checkpoint reads (R2.1) is therefore exactly what the commits through the epoch's
last commit recorded, on every validator.

**[I]** The table is written directly, not through `ConsensusCommitOutput`, so it escapes the
consensus quarantine. A replay after a restart records the same notifications again, which the
generation rule makes harmless.

---

## 2. Choosing the next version

### R2.1 [E] Where: building the epoch's last checkpoint

`create_and_execute_advance_epoch_tx` (AUTH:6033-…) calls
`choose_protocol_version_and_system_packages_v2(epoch_store.protocol_version(),
epoch_store.protocol_config(), epoch_store.committee(), epoch_store.get_capabilities_v2(),
epoch_store.get_effective_buffer_stake_bps())` (AUTH:6090-6100).

`get_effective_buffer_stake_bps` (EPS:2678-2689) is the epoch config's
`buffer_stake_for_protocol_upgrade_bps` (5000 from protocol 6 on, PC:3154), unless an operator set
an override through `set_override_protocol_upgrade_buffer_stake` (EPS:2656, an admin
endpoint). **(uncertain)** whether an override is used on any live network; it is local state,
so validators with different overrides would disagree.

### R2.2 [E] The rule

`choose_protocol_version_and_system_packages_v2` (AUTH:5621-5645) starts from the current version
and, while `is_protocol_version_supported_v2(current, next + 1, …)` returns a version and packages,
advances to it. It returns the last `(version, packages)` found, or the current version and no
packages.

`is_protocol_version_supported_v2` (AUTH:5533-5619), for a proposed version `p`:

1. If `p > current + 1` and `advance_to_highest_supported_protocol_version` is off, none (on from
   protocol 9, PC:3173, so versions may be skipped).
2. Clamp `buffer_stake_bps` to 10000.
3. For each recorded capability: drop it if it lists no system packages (a vote against any
   change); sort its packages; drop it if it has no digest for `p`; else keep
   `(digest, packages, authority)`.
4. Sort, and group by `(digest, packages)`. For each group, sum its stake (`StakeAggregator`,
   counting each authority once). With `quorum = committee.quorum_threshold()` and
   `f = total - quorum`, the group wins if its stake ≥ `quorum + ceil(f * buffer_bps / 10000)`.
   The first winning group gives `(p, packages)`. At most one group can win.

With a total of 10,000 and 5000 bps: quorum 6,667, f 3,333, threshold 6,667 + 1,667 = 8,334.

### R2.3 [E] Packages to write

`get_system_package_bytes` (AUTH:5441-…) turns the winning references into
`(version, module bytes, dependencies)` triples, skipping every package whose on-chain object
already has that reference (AUTH:5450-5457). A version-only upgrade, where the binary's framework
equals the chain's, therefore carries no packages. If a needed package is not in the binary, the
checkpoint builder cannot make the transaction and retries forever (`SystemPackagesMissing`,
AUTH:6113-6138); the validator gets the last checkpoint from others by state sync instead.

### R2.4 [E] The transaction

The version and packages go into the last `EndOfEpochTransactionKind::ChangeEpoch`
(`new_change_epoch(next_epoch, next_epoch_protocol_version, storage_cost, computation_cost,
storage_rebate, non_refundable_storage_fee, epoch_start_timestamp_ms,
next_epoch_system_package_bytes)`, AUTH:6141-6150), or a bare `ChangeEpoch` in protocol versions
without end-of-epoch transactions. `ChangeEpoch.protocol_version` is "the protocol version in
effect in the new epoch" and `system_packages` the packages written before it starts
(TX:320-341). The last checkpoint's summary carries `next_epoch_protocol_version` in its
`end_of_epoch_data`, read back from the system state after execution.

---

## 3. The epoch boundary

### R3.1 [E] Executing `ChangeEpoch`

`advance_epoch` (EXE:1975-2047) runs `0x3::sui_system::advance_epoch` with
`next_protocol_version` (EXE:1987-1998, the argument at EXE:1889), which records the version in the
system state. If that call fails, it drops the writes and enters safe mode
(`advance_epoch_safe_mode`, EXE:2014-2027), which still records the epoch and version.

Then, with a fresh Move runtime for the current config, `process_system_packages`
(EXE:2048-…) writes each package: one at `OBJECT_START_VERSION` is published by a system
`Publish` command (EXE:2067-2090); any other replaces the existing package object at its new
version, built with `Object::new_system_package` and the change-epoch transaction as its previous
transaction (EXE:2091-…). The package digests every validator commits to (R1.3) are computed on
the same rule, so the executed result matches the voted references.

### R3.2 [E] The next epoch's `ProtocolConfig`

The next epoch's start state comes from the system state the change-epoch transaction wrote
(`into_epoch_start_state`), and the new epoch store builds
`ProtocolConfig::get_for_version(epoch_start_state.protocol_version(), chain)` (EPS:893-907).

### R3.3 [E] A version the binary does not support

`AuthorityState::check_protocol_version` (AUTH:3376-3397), called when the authority state is
made and on reconfiguration with the node's supported range (NODE:2347-2352), logs and exits
(`std::process::exit(1)`; in simulation, the node shuts down) if the epoch's version is outside
the range. `ProtocolConfig::get_for_version` itself asserts `version <= MAX_ALLOWED` (PC:2517).

`ProtocolVersion::MAX` is 138 (PC:41). Simulation builds add a fake `MAX_ALLOWED = MAX + 1`
(PC:426-431) to test upgrades; release builds have `MAX_ALLOWED = MAX`.

---

## 4. Safe mode

Safe mode (R3.1) keeps the chain alive when `advance_epoch` aborts: the epoch and version still
advance, rewards are not distributed. It is execution's concern; a reimplementation that runs the
reference's execution engine gets it for free. **[E]** only that its effects match.

---

## 5. Nothing in an epoch is keyed on the next version

Consensus (`consensus-core` gets the epoch's config at its start), block verification, the commit
handler and execution all read the epoch's own `ProtocolConfig`. The capabilities matter only to
the change-epoch transaction. Confirmed by the reads above: the only consumer of
`get_capabilities_v2` is `create_and_execute_advance_epoch_tx` (AUTH:6098).

---

## 6. Anchovy today, and the gaps

### What anchovy does

- **Decoding and voting.** `messages` decodes `CapabilityNotificationV2`
  (`build/consensus.rs:64`, `:134`). Voting accepts it without checks and rejects V1, as the
  reference does (`validator/src/consensus/vote.rs:392-404`). The commit decoder drops it
  (`validator/src/consensus/decode.rs:347`, `_ => Decoded::Other`): nothing is recorded.
- **Submitting.** Anchovy never submits capabilities.
- **The change-epoch transaction.** `checkpoint/src/close.rs:123-129` calls
  `validator::consensus::commit::end_of_epoch_transaction` (`commit.rs:1174-1200`) with the
  epoch's own version (`epoch.config.version`) and always `system_packages: Vec::new()`.
- **The next epoch.** `checkpoint/src/close.rs:398-418` reads the system state after the
  change-epoch transaction and puts its `protocol_version` into the summary's end-of-epoch data;
  `node/src/epoch.rs` keeps it in the epoch record, and `validator/src/epoch.rs:36` builds the next
  epoch's config with `ProtocolConfig::get_for_version`. So anchovy follows whatever version its
  change-epoch transaction wrote, which today is always the current one.
- **Execution.** Anchovy's executor ports `advance_epoch` and `process_system_packages`
  (`executor/src/execution_engine.rs:1331`, `:1400`), so it would apply packages it was given.
- **Supported range.** None: no check that the epoch's version is supported. `get_for_version`'s
  assertion would panic on a version above 138.
- **Two `ProtocolConfig`s.** The validator uses anchovy's copy (`crates/protocol-config`, `MAX`
  138); execution uses the reference's `sui-protocol-config`.

### Gaps, by consequence in a mixed network

1. **Anchovy's stake never supports an upgrade.** Sui validators see no capabilities from
   anchovy, so its stake is in no group (R2.2). In the compat network (sui 6,668 of 10,000) no
   upgrade can reach the 8,334 threshold, so none happens and nothing forks, but the network can
   never upgrade. With enough sui stake alone (≥ 8,334), sui validators upgrade without anchovy.
2. **Anchovy keeps the version.** When sui validators choose a new version or packages, anchovy's
   change-epoch transaction differs from theirs: the last checkpoint forks (anchovy's aggregator
   panics on the certified checkpoint), and anchovy would start the next epoch at the old
   version.
3. **Anchovy does not record capabilities.** Even with a choice rule, anchovy has no set to
   choose from. It needs R1.5's author check, R1.6's last-generation rule, the cut-off when
   certificates close, and keeping the set across a restart (in the epoch store's quarantined
   output, or re-recorded by replay).
4. **Digests and packages must match sui's exactly** for anchovy's own capabilities to count with
   sui's: the BCS of the reference's `ProtocolConfig` per version (anchovy's copy must serialize
   identically, or anchovy computes digests with `sui-protocol-config`), and the references
   `compare_system_package` gives over anchovy's store (it takes any `ObjectStore`; anchovy's
   `StoreView` already serves `get_sui_system_state`).
5. **Package bytes.** For a framework upgrade, anchovy needs `get_system_package_bytes`'s rule
   (skip unchanged packages, re-derive each from its predecessor's previous transaction) to build
   an identical `ChangeEpoch`.
6. **Unsupported versions.** Anchovy should exit, as R3.3, rather than panic deep in
   `get_for_version`.

### Exercising an upgrade in the compat harness

- **Version-only upgrade, no simulator needed.** Start genesis one version below the maximum
  (`ConfigBuilder::with_protocol_version(ProtocolVersion::new(MAX - 1))`, SWC:275-280; genesis
  defaults to `MAX`, `crates/sui-config/src/genesis.rs:406`) with the default supported range
  `MIN..=MAX`. The binary's framework equals genesis's, so the packages are unchanged and
  `ChangeEpoch.system_packages` is empty (R2.3). With anchovy's stake needed for the threshold
  (gap 1), the network upgrades to `MAX` at the first epoch end only if anchovy submits matching
  capabilities, and anchovy must then write `MAX` in its change-epoch transaction.
- **Per-validator ranges.** `ConfigBuilder::with_supported_protocol_versions` (one range for all,
  SWC:282-285) and `with_supported_protocol_version_callback` (per validator, SWC:287-293) set what
  each sui validator advertises, to test a split vote (no upgrade) or one validator lacking a
  version.
- **Config overrides.** `ProtocolConfig::apply_overrides_for_testing` (PC:4956-…) changes configs
  in the process, which would change the digests (R1.2): anchovy and sui in one process see the
  same override only if anchovy computes digests with `sui-protocol-config`.
- **Framework upgrades** need different framework bytes, which the reference tests only in
  simulation (`framework_injection`, `crates/sui-e2e-tests/tests/protocol_version_tests.rs`, with
  the fake `MAX_ALLOWED` version, R3.3). **(uncertain)** whether the compat harness can do one
  outside simulation; it would need a second set of system package bytes.
