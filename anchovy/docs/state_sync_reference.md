# Sui Checkpoint State Sync and Synced-Checkpoint Execution: Behavioral Reference

This document specifies how the reference Sui node serves and fetches certified checkpoints over
anemo (the `StateSync` service), how it verifies what it fetches, and how the checkpoint executor
runs a checkpoint that the node did not build. It is written so that anchovy can (a) serve sui
fullnodes and validators on the wire, (b) catch up an epoch whose consensus is gone, and (c)
execute synced checkpoints producing byte-identical effects.

Requirements are numbered (R<section>.<n>) and tagged as in `reconfiguration_reference.md`:
**[E]** essential (needed for wire compatibility, agreement/no fork, safety or liveness), **[I]**
incidental, **[E/I]** essential property, incidental mechanism. Uncertain statements are marked
**(uncertain)**. `ABBREV:line` references are relative to the repo root at commit `551307cf97`;
`anchovy/...` paths are relative to the anchovy worktree. The reconfiguration reference is cited as
**RCF R<n>**, the randomness reference as **RND R<n>**, the writeback cache reference as **WBR §n**.
Behaviour those documents already specify (the final checkpoint, the change-epoch tx, the root
state hash, reconfiguration, crash windows) is cited, not repeated.

| Abbrev | File |
|---|---|
| SS | `crates/sui-network/src/state_sync/mod.rs` |
| SSV | `crates/sui-network/src/state_sync/server.rs` |
| SSB | `crates/sui-network/src/state_sync/builder.rs` |
| SSW | `crates/sui-network/src/state_sync/worker.rs` |
| NB | `crates/sui-network/build.rs` |
| DISC | `crates/sui-network/src/discovery/mod.rs` |
| P2P | `crates/sui-config/src/p2p.rs` |
| NCFG | `crates/sui-config/src/node.rs` |
| CODEC | `crates/mysten-network/src/codec.rs` |
| NODE | `crates/sui-node/src/lib.rs` |
| STOR | `crates/sui-core/src/storage.rs` (`RocksDbStore`, the state-sync store) |
| SL | `crates/sui-storage/src/lib.rs` |
| CP | `crates/sui-core/src/checkpoints/mod.rs` |
| CPO | `crates/sui-core/src/checkpoints/checkpoint_output.rs` |
| CE / CEU | `crates/sui-core/src/checkpoints/checkpoint_executor/mod.rs` / `utils.rs` |
| EPS | `crates/sui-core/src/authority/authority_per_epoch_store.rs` |
| SOVM | `crates/sui-core/src/authority/shared_object_version_manager.rs` |
| ESI | `crates/sui-core/src/execution_scheduler/execution_scheduler_impl.rs` |
| ED | `crates/sui-core/src/execution_driver.rs` |
| AUTH | `crates/sui-core/src/authority.rs` |
| AST | `crates/sui-core/src/authority/authority_store.rs` |
| PRN | `crates/sui-core/src/authority/authority_store_pruner.rs` |
| WBC | `crates/sui-core/src/execution_cache/writeback_cache.rs` |
| COMS | `crates/sui-core/src/epoch/committee_store.rs` |
| AFS / AWP | `crates/sui-core/src/accumulators/design_docs/address_funds_scheduling.md` / `write_path.md` |
| MCK | `crates/sui-types/src/messages_checkpoint.rs` |
| CRY | `crates/sui-types/src/crypto.rs` |
| COM | `crates/sui-types/src/committee.rs` |
| DIG | `crates/sui-types/src/digests.rs` |
| BT | `crates/sui-types/src/base_types.rs` |
| FX / FX2 | `crates/sui-types/src/effects/mod.rs` / `effects_v2.rs` |
| TX | `crates/sui-types/src/transaction.rs` |
| XP | `crates/sui-types/src/execution_params.rs` |
| NR | `crates/sui-types/src/node_role.rs` |
| SUIT | `crates/sui-types/src/lib.rs` |
| PC | `crates/sui-protocol-config/src/lib.rs` |
| AP2P | `anchovy/crates/node/src/p2p.rs` (the uncommitted working-tree version, not HEAD) |
| ANEMO | anemo at the pinned rev `68adc31` (`Cargo.toml:649`), `crates/anemo/src/routing/mod.rs` |

Terms:

- **Summary**: a `CertifiedCheckpointSummary` = `Envelope<CheckpointSummary,
  AuthorityStrongQuorumSignInfo>` (MCK:507-512). **Contents**: `CheckpointContents` (digests and
  pinned user signatures, MCK:594-632). **Full contents**: `VersionedFullCheckpointContents`
  (contents plus every transaction and its effects, MCK:892-993).
- **Highest verified / synced / executed**: the three checkpoint watermarks (R3.8).

---

## 0. Overview

1. Every sui node (validator or fullnode) runs one `StateSync` instance on its p2p anemo network
   for its whole lifetime; it never reconfigures (RCF R4.14). It serves five routes and runs a
   loop that learns peers' heights, downloads and verifies summaries in order, then downloads full
   contents, and finally bumps `highest_synced`.
2. The checkpoint executor consumes checkpoints in order once they are synced (RCF R3.1). On a
   fullnode, and on a validator for the final checkpoint or any checkpoint it has not built itself,
   it executes the checkpoint's transactions one by one (the **synced path**). Each transaction's
   consensus-object versions come from its certified effects. Execution must reproduce the
   certified effects digest, or the node halts.
3. On a validator, certified checkpoints that it aggregated itself enter state sync through
   `Handle::send_checkpoint`. State sync bumps both watermarks and pushes the summary to peers.
4. Anchovy today serves only `/sui.Randomness/SendSignatures` (AP2P:98). Every `StateSync` route
   answers anemo `NotFound`. Sui peers then treat the anchovy validator as a non-participant in
   state sync (R2.12).

---

## 1. The StateSync anemo service

### 1.1 Transport and codec

**R1.1 [E] Service and package.** The service is generated by `anemo_build` with package `sui`
and service name `StateSync` (NB:174-222). Route paths are `/sui.StateSync/<RouteName>`. It is
merged into the same anemo router as `Discovery` and `Randomness` (NODE:1277-1281), on the p2p
listen address (default `0.0.0.0:8084`, P2P:44-46), with server name `sui-<chain id>`
(NODE:1366-1372). Anchovy already reproduces the network setup (AP2P:79-117).

**R1.2 [E] Codec.** All routes use `mysten_network::codec::anemo::BcsSnappyCodec` (NB:149,
NB:181-220): the body is the BCS of the message, written through a snappy **frame** encoder
(CODEC:167-171). Decoding decompresses with a 128 MiB cap, then BCS-decodes (CODEC:11-28,
CODEC:183-184). Anchovy's `BcsSnappy` is the same codec (AP2P:340-404).

**R1.3 [E] Frame limits.** Sui sets anemo `max_request_frame_size = 1 MiB` and
`max_response_frame_size = 128 MiB` (NODE:1319-1325). Full contents are sent as one response, so a
full-contents response larger than 128 MiB cannot be delivered. AP2P:37-39 matches.

### 1.2 Routes

All five routes, in declaration order (NB:176-221). Types are the sui-types BCS types.

| Route | Request | Response |
|---|---|---|
| `/sui.StateSync/PushCheckpointSummary` | `CertifiedCheckpointSummary` | `()` |
| `/sui.StateSync/GetCheckpointSummary` | `GetCheckpointSummaryRequest` | `Option<CertifiedCheckpointSummary>` |
| `/sui.StateSync/GetCheckpointContents` | `CheckpointContentsDigest` | `Option<FullCheckpointContents>` |
| `/sui.StateSync/GetCheckpointContentsV2` | `CheckpointContentsDigest` | `Option<VersionedFullCheckpointContents>` |
| `/sui.StateSync/GetCheckpointAvailability` | `()` | `GetCheckpointAvailabilityResponse` |

**R1.4 [E] Wire types defined in `sui-network`.** These are defined in SSV:23-34:

- `enum GetCheckpointSummaryRequest { Latest, ByDigest(CheckpointDigest),
  BySequenceNumber(u64) }`. The BCS variant indices are 0, 1 and 2.
- `struct GetCheckpointAvailabilityResponse { highest_synced_checkpoint:
  CertifiedCheckpointSummary, lowest_available_checkpoint: u64 }`.
- `CheckpointDigest` and `CheckpointContentsDigest` wrap `Digest([u8; 32])`, which serializes
  through `serde_with::Bytes` (DIG:20-28, DIG:279-282, DIG:378-379). In BCS this is a
  length-prefixed byte string (`0x20` followed by 32 bytes), not a fixed array.
- Note: anchovy's private `GetCheckpointSummaryRequest` declares `ByDigest([u8; 32])`
  (AP2P:247-254). That would encode without the length byte. The variant is unused today, but it
  must be fixed before it is used.

**R1.5 [E] `PushCheckpointSummary` (SSV:48-100).**

1. It requires the anemo peer id. Without one it answers `Status::internal`.
2. `peer_heights.update_peer_height(peer, seq)` raises the pusher's known height. It returns false
   if the pusher has no entry in the local peer table, or is marked as being on another chain
   (SS:368-385). **In that case the push is dropped and the handler still answers `Ok(())`**
   (SSV:71-74). A peer gets an entry only after this node has fetched the peer's genesis summary
   (R2.1). So a push from a peer this node has not yet probed has no effect.
3. If `seq <= highest_verified + max_checkpoint_lookahead` (default 1000, P2P:346-351), the
   summary is stored, **unverified**, in the in-memory `unprocessed_checkpoints` map, at most one
   per sequence number. A second digest for the same sequence number is dropped (SS:398-420).
   Otherwise it is not stored, but the height still counts.
4. If `seq > highest_verified`, it sends `StartSyncJob` to the event loop (SSV:93-97).

The response is always `()`. The push carries no verification and no reply semantics. Its value
is that it bumps the peer's height and pre-fills the summary cache.

**R1.6 [E] `GetCheckpointSummary` (SSV:102-122).**

- `Latest` returns `get_highest_synced_checkpoint()` (always `Some`; the store is initialized with
  genesis, STOR:126-134).
- `ByDigest(d)` reads `checkpoint_by_digest`.
- `BySequenceNumber(n)` reads `certified_checkpoints`.
- Missing data is `Ok(None)`, not an error status.

The pruner deletes contents, transactions, effects and `checkpoint_by_digest` entries, but **not**
`certified_checkpoints` (PRN:349-373). So `BySequenceNumber` keeps answering for pruned
checkpoints, while `ByDigest` stops answering for them.

`BySequenceNumber(0)` is how every peer checks that a node is on the same chain (R2.1). A server
must always answer it with the genesis summary. Answering `None` gets the server marked as being
on another chain; answering with an error makes it invisible (R2.12).

Note that this route serves certified summaries above `highest_synced`, that is, summaries that
are verified but whose contents are not downloaded yet. Only `Latest` is bounded by
`highest_synced`.

**R1.7 [E] `GetCheckpointContents` (v1) and `GetCheckpointContentsV2` (SSV:144-162).** Both call
`get_full_checkpoint_contents(None, digest)` (STOR:148-224):

1. Look up the sequence number through `checkpoint_sequence_by_contents_digest`. If it is present,
   return `full_checkpoint_content_v2[seq]`. That table holds contents that arrived through state
   sync, and is kept for 5000 checkpoints after execution (CE:1087-1114).
2. Otherwise, assemble the full contents from `checkpoint_content[digest]` plus each transaction
   and its effects, read through the cache. If any of them is missing, return `None`
   (STOR:192-223). Locally built checkpoints on a validator are always served this way: the
   builder writes only `checkpoint_content` and `locally_computed_checkpoints` (CP:1850-1858),
   never `full_checkpoint_content_v2`.

The result keeps the contents' version: V1 contents give `VersionedFullCheckpointContents::V1`,
V2 contents give `V2` (MCK:902-921).

- v2 returns the versioned value as is.
- v1 returns `into_v1()`, which drops the `Option<SequenceNumber>` alias version from each V2
  signature (MCK:946-958). For V2 contents, the v1 answer **cannot be verified** by the client:
  `FullCheckpointContents::checkpoint_contents()` rebuilds V1 contents, whose digest differs from
  the summary's V2 `content_digest` (MCK:1060-1066, MCK:925-932).
- Checkpoints use V2 contents whenever `address_aliases` is on (CP:2126-2136). That is the case
  on every chain from protocol version 116 (PC:4526-4528; earlier on non-mainnet, PC:4458-4459).
  Clients default to v2 (`use_get_checkpoint_contents_v2 = true`, P2P:339-344; SS:1875-1881).

So v1 is legacy. A NotFound status on v1 is harmless for current clients **(uncertain:** old
binaries with `use-get-checkpoint-contents-v2: false` would fail, but they cannot verify
current checkpoints anyway**)**.

**R1.8 [E] `GetCheckpointAvailability` (SSV:124-142).** It returns `highest_synced_checkpoint`
(the full certified summary at the highest-synced watermark) and `lowest_available_checkpoint`.
The latter is `highest_pruned + 1`, or 0 if the node never pruned (STOR:136-146). The
`HighestPruned` watermark is written by the checkpoint pruner, together with deleting that
checkpoint's contents, transactions and effects (PRN:349-373). Pruning is off unless
`num_epochs_to_retain_for_checkpoints` is set (NCFG:1394, NCFG:1439). Store errors are returned as
`Status::internal`.

**R1.9 [E] Meaning of the two watermarks a server exposes.**

- **Highest synced** is the server's guarantee of availability. For every `n <= highest_synced`,
  the server has the summary and the full contents of `n`. It does not necessarily have objects,
  and the checkpoint is not necessarily executed (SS:19-27). Peers use it as the server's height:
  they ask it for summaries and contents up to that height (SS:580-588).
- **Lowest available** is the lowest checkpoint whose contents the server still has. Peers do not
  ask for contents below it (SS:584-585).
- A server whose answers break these claims gets failures recorded against it (R2.9).

### 1.3 Limits

**R1.10 [I] Request size limit.** A route layer drops any request whose (compressed) body is
larger than `max_checkpoint_summary_size` (default 256 KiB, P2P:360-365). It answers with an
**empty success response**, without calling the handler (SSV:165-225, SSB:128-135). Only a pushed
summary can be that large.

**R1.11 [I] Rate, inflight and per-checkpoint limits are all off by default.** These limits exist:

- per-peer rate limits on push, summary and contents, in block mode (requests wait);
- a per-peer inflight limit on `GetCheckpointContents` that returns an error status;
- a global per-contents-digest semaphore on `GetCheckpointContents` that answers
  `TooManyRequests` when exhausted. Its map is pruned at 5000 entries on each tick (SSV:227-328,
  SS:983-985).

All default to `None` (P2P:166-195, SSB:88-126). No production default config sets them (no other
setter in `crates/` or `nre/`). The contents layers are attached only to the v1 route
(`add_layer_for_get_checkpoint_contents`). **(uncertain:** the v2 route appears to have no
limits, even when they are configured**.)**

**R1.12 [E/I] The server checks nothing about the requester.** Get routes answer any connected
peer, on any chain. Only the push path consults the peer table (R1.5). Anemo answers `NotFound`
for an unregistered route (ANEMO:80, ANEMO:264). That is what anchovy returns today for every
StateSync route.

---

## 2. Who calls the routes, and when

All callers are in the state-sync event loop and its tasks (SS:704-1123). Validators and fullnodes
run the same code with the same `RocksDbStore` (NODE:706, NODE:1251-1257). Timeouts below come
from `StateSyncConfig` defaults (P2P:268-411).

### 2.1 Learning a peer's chain and height

**R2.1 [E] Chain check on connect.** On `PeerEvent::NewPeer`, and for every peer connected at
start-up, the loop runs `get_latest_from_peer` (SS:717-724, SS:938-940, SS:957-972, SS:1150-1219).
For a peer without an entry:

1. It sends `GetCheckpointSummary(BySequenceNumber(0))` with a 10 s timeout (P2P:309-315).
2. `Ok(Some(cp))` inserts `PeerStateSyncInfo { genesis_checkpoint_digest: cp.digest(),
   on_same_chain_as_us: digest == our genesis digest, height: cp.seq (0), lowest: 0 }`.
3. `Ok(None)` inserts the peer as **not on the same chain**.
4. `Err(status)` (any status, including `NotFound`, or a timeout) **returns without inserting
   anything**. The peer is not probed again until it disconnects and reconnects. Only `NewPeer`
   triggers a probe; the periodic tick queries only peers that already have a same-chain entry
   (SS:1282-1287).

If the peer is on the same chain, step 5 follows at once: `GetCheckpointAvailability`, falling back
to `GetCheckpointSummary(Latest)` (R2.2), which sets the height and lowest watermark. On
`LostPeer`, the entry and the peer's score are removed (SS:941-945).

**R2.2 [E] Availability query and its fallback.** `query_peer_for_latest_info` (SS:1229-1272):

1. Send `GetCheckpointAvailability`. On success, the result is `(highest_synced_checkpoint,
   Some(lowest_available))`.
2. On an error status other than `NotFound`, the result is a failure.
3. On `NotFound` (a peer that predates the route), send `GetCheckpointSummary(Latest)`. `Some(cp)`
   gives `(cp, None)`. The peer's `lowest` then stays at its default of **0**, which means "has
   everything". `None` is neutral. An error is a failure.

`update_peer_info` raises the height monotonically and stores the summary in
`unprocessed_checkpoints`, unverified (SS:315-335).

**R2.3 [E/I] Polling.** Every `interval_period` (5 s, P2P:269-273), `handle_tick` spawns
`query_peers_for_their_latest_checkpoint` over all connected same-chain peers, with a 10 s timeout
each (SS:974-988, SS:1275-1350).

- A success records a scoring success, weighted by the summary's BCS size and the latency
  (SS:1297-1306).
- A failure records a scoring failure (SS:1315-1318).
- If the highest height returned is above the highest known height, `StartSyncJob` is sent
  (SS:1341-1349).

### 2.2 Pushing

**R2.4 [E/I] Who pushes, and to whom.** `notify_peers_of_checkpoint` (SS:1125-1148) sends
`PushCheckpointSummary(summary)` with a 10 s timeout. It goes to every connected same-chain peer
whose known height is **below** the checkpoint. Responses are ignored. It is triggered in two
places:

- **Validators, for each certified checkpoint they aggregate.** `SendCheckpointToStateSync`
  (CPO:199-217, NODE:1716) feeds `handle_checkpoint_from_consensus` (SS:842-929), which:
  1. panics if checkpoint `seq-1` is missing from `certified_checkpoints`, or if its
     `previous_digest` does not match it (SS:845-855);
  2. ignores the checkpoint if `highest_verified >= seq` (SS:857-865). It is then not pushed;
  3. inserts the next epoch's committee if the checkpoint is the final one (SS:905-916);
  4. sets **both** `highest_verified` and `highest_synced` to it (SS:918-923);
  5. broadcasts it to local subscribers (SS:926), and pushes it (SS:928).

  The caller must guarantee that the contents, transactions and effects are already persisted
  (SS:111-124). The highest-synced claim depends on this.
- **Any node, after syncing contents from peers.** The contents task sends
  `SyncedCheckpoint(highest_synced)` to the loop, which pushes it (SS:834-836). It does so when
  `highest_synced` is a multiple of the content download concurrency (400), or when no content
  download is in flight (SS:1764-1774). So a node that is catching up pushes in steps of about
  400, and a node at the tip pushes every checkpoint.

**R2.5 [I] Push is best effort.** There is no retry and no acknowledgement. Peers also learn
heights by polling every 5 s (R2.3). A missing push only adds latency.

### 2.3 Downloading summaries

**R2.6 [E] Summary sync (SS:1021-1078, SS:1352-1544).**

- At most one summary task runs at a time. Its target is `min(highest known peer height,
  highest_verified + max_checkpoint_sync_batch_size (400))` (P2P:353-358). If the target was
  capped, it restarts itself on success.
- For each `next` from `highest_verified + 1` to the target, with up to 400 concurrent requests
  (`checkpoint_header_download_concurrency`, P2P:288-293), consumed **in order**:
  1. Use the summary from `unprocessed_checkpoints` if there is one. It is unattributed: it came
     from a push or a poll (SS:1391-1397).
  2. Otherwise, try peers from a `PeerBalancer` (R2.8) that claim `height >= next`. Send
     `GetCheckpointSummary(BySequenceNumber(next))` with a 10 s timeout. `None` or an error records
     a failure and the next peer is tried (SS:1401-1427).
  3. A summary whose sequence number is not `next`, or whose digest differs from a configured
     pinned checkpoint, gets the peer **marked as not on the same chain** (SS:1432-1463).
- Each summary is verified against the previous one (R3.2), or accepted unverified if it is pinned
  (SS:1492-1498). It is then inserted with `store.insert_checkpoint`, which writes the summary,
  inserts the next committee if the summary is final, and bumps `highest_verified` (STOR:352-370,
  CP:863-869). A verification failure removes the cached summary and marks the peer that supplied
  it, if any, as not on the same chain. Then the task ends with an error (SS:1499-1516).
- If no peer can supply `next`, the task ends with an error, and the next tick or push starts a new
  one.

### 2.4 Downloading contents

**R2.7 [E] Contents sync (SS:1647-1920).** A single long-lived task downloads contents for
`highest_synced+1 ..= highest_verified`. It runs only while some peer claims a height above
`highest_synced` (SS:1080-1112).

- Up to 400 checkpoints are in flight (P2P:295-300), with at most 50,000 transactions in flight
  in total (P2P:302-307, SS:1744-1750). Results are consumed in order (`FuturesOrdered`).
- For each checkpoint, `sync_one_checkpoint_contents` (SS:1779-1847):
  1. If `highest_synced >= seq` (for example, it was produced by consensus), it is done.
  2. If the store already has full contents, it is done (SS:1863-1866).
  3. Otherwise, try peers with `height >= seq && lowest <= seq`. Send `GetCheckpointContentsV2(
     content_digest)`, with an adaptive timeout of 5 s + (30 s − 5 s) · min(tx_count/10,000, 1),
     ±10 % jitter (SS:137-153, P2P:367-378).
  4. A response passes only if `verify_digests(content_digest)` holds (R3.5). It is then inserted
     (`insert_checkpoint_contents`, R3.7).
  5. `None`, an error, or a digest mismatch records a failure. A digest mismatch does **not** mark
     the peer as being on another chain (SS:1884-1916).
  6. If no peer succeeds, it sleeps until `wait_interval_when_no_peer_to_sync_content` (10 s,
     P2P:328-337) has passed since the start, then retries at the front of the queue
     (SS:1832-1843, SS:1698-1728).
- On success, `highest_synced` is set to the checkpoint and it is broadcast to subscribers
  (SS:1686-1696).

Note that `checkpoint_content_timeout` (60 s, P2P:317-322) is unused, and that the doc comments
for the timeouts (P2P:156, P2P:162, P2P:230) disagree with the code defaults.

### 2.5 Peer selection, scoring and dropping

**R2.8 [I] `PeerBalancer` (SS:510-667).** Same-chain connected peers fall into three groups:

- *failing* peers (R2.9);
- *known* peers, which have a throughput score: bytes over response time in the scoring window;
- *unknown* peers, sorted by RTT.

With probability `exploration_probability` (0.1), or when no known peer is eligible, the closest
unknown eligible peer is tried. Otherwise a known peer is picked at random, weighted by
throughput. Failing peers are tried last. Each peer is tried at most once per request.

**R2.9 [E/I] Scoring and disconnect (SS:155-266, SS:461-507, SS:990-1019).**

- The score keeps the last 20 successes and failures. A peer is *failing* when there are at least
  10 samples in the last 60 s and the failure rate is ≥ 0.3 (P2P:380-402).
- A peer that has been failing continuously for `peer_disconnect_threshold` (30 s, P2P:407-411)
  is reported to discovery, at most one peer per tick. Discovery disconnects it and puts it on a
  1 h dial cooldown (DISC:490-510, P2P:576-579).
- Discovery ignores the report for *trusted* peers: configured seed or allowlisted peers, and the
  `chain_peers` set (DISC:491-494, DISC:1466-1472). It also ignores the report when fewer than
  `min_peers_for_disconnect` peers are connected. **(uncertain:** `chain_peers` appears to be the
  on-chain validator set, which would mean sui never drops a validator for state-sync failures.**)**
- These count as failures: answering `None` to a summary or contents request the peer claimed it
  could serve; any error status, including `TooManyRequests` and `NotFound` (except on
  `GetCheckpointAvailability`, R2.2); timeouts; contents that fail `verify_digests`.

**R2.10 [E] Wrong chain or a bad summary.** A peer is marked `on_same_chain_as_us = false`, which
excludes it from every selection, poll and push until it reconnects, in these cases:

- its genesis summary differs from ours, or it answers `None` for sequence 0 (R2.1);
- it returns a summary with the wrong sequence number, or one that contradicts a pinned digest
  (R2.6);
- it supplied a summary that fails verification (R2.6).

A bad summary that arrived through a push or a poll is unattributed. It is discarded, and the
pusher is not punished (SS:1508-1510).

### 2.6 Archive fallback

**R2.11 [I] Archive fallback (SS:1546-1645, SSW:16-85).** Every 5 s, if `highest_synced` is below
the minimum `lowest` over all same-chain peers, the task fetches checkpoints `highest_synced+1 ..=
that minimum` from the configured `archive_reader_config().ingestion_url` (an object store). It
refuses `checkpoints.mainnet.sui.io` (SS:1615-1618).

For each checkpoint it verifies the summary (R3.2) and inserts it, verifies the contents' digests,
inserts them, and bumps `highest_synced`. This is how a node gets past history that all its peers
have pruned.

### 2.7 What sui expects of a validator peer

**R2.12 [E] Effect of the current anchovy behaviour (NotFound on every route).**

1. Each sui node that connects to anchovy sends one `GetCheckpointSummary(BySequenceNumber(0))`.
   It gets `NotFound`, which is an `Err`, and inserts no entry (R2.1). From then on, for that
   connection, anchovy is not in that node's peer table. It is never polled, never pushed to, never
   asked for summaries or contents, and never scored or disconnected for state sync. It is simply
   invisible.
2. Anchovy's own pushes, if it made any, would be dropped silently (R1.5).
3. Nothing breaks on the sui side, as long as each sui node has other peers that serve state sync.
   The degradations are:
   - A sui validator that lags at an epoch boundary relies on state sync to finish that epoch
     (R5.3). It can sync only from peers that serve. If most validators are anchovy and its
     fullnode peers do not have the data, it stalls.
   - Fullnodes whose only peers are anchovy validators cannot sync at all.
   - Certified checkpoints that anchovy aggregates are not propagated by anchovy. Other validators
     certify the same checkpoints themselves, so validators are not affected.
4. Anchovy itself cannot sync (R6 scenario ii). It already calls `GetCheckpointSummary` and
   `GetCheckpointContentsV2` as a client, for randomness recovery (AP2P:257-288). The server side
   does not check the requester's chain or peer table, so this works today (R1.12).

**R2.13 [E] What a serving validator must provide, to be a good peer.**

- Always answer `BySequenceNumber(0)` with the genesis summary, from the moment it accepts
  connections. A failed first probe makes it invisible until the peer reconnects (R2.1).
- Answer `GetCheckpointAvailability` with a `highest_synced` whose contents it can actually serve,
  and with an honest `lowest_available`. If it answers this route with `NotFound`, peers fall back
  to `Latest` and assume `lowest = 0` (R2.2). They then request pruned contents and record
  failures.
- Answer `BySequenceNumber(n)` for every `n <= highest_synced`. If it answers `None` for a
  checkpoint it claimed, the peer records a failure.
- Answer `GetCheckpointContentsV2` for every `n` in `[lowest_available, highest_synced]`, with
  bytes that pass `verify_digests` (R3.5). Responses must fit in 128 MiB (R1.3).
- Push each newly certified checkpoint (R2.4). This is optional for correctness, but it is the
  low-latency path. Without it, peers learn of new checkpoints by polling every 5 s.
- Accept pushes. It may ignore them, but it must answer `()`. A NotFound status on a push is
  ignored by the sender (SS:1142-1147).

---

## 3. Verifying synced checkpoints

### 3.1 Summaries and committees

**R3.1 [E] Which committee.** A summary for epoch `e` is verified against the committee for `e`
from the committee store (`store.get_committee(checkpoint.epoch())`, SL:220-237, STOR:226-228).

- The genesis committee is installed when the store is created (COMS:37, COMS:65).
- The committee for `e+1` is learned **only** from the `end_of_epoch_data.next_epoch_committee` of
  epoch `e`'s final checkpoint, once that checkpoint has itself been verified. It is built as
  `Committee::new(e+1, next_epoch_committee.collect::<BTreeMap>())`, which sorts by
  `AuthorityName` and asserts that the stake sums to `TOTAL_VOTING_POWER` = 10,000 (COM:58-77,
  COM:40). It is inserted by `insert_checkpoint` (STOR:356-365), or by
  `handle_checkpoint_from_consensus` for validator-built checkpoints (SS:905-916).
- `insert_new_committee` is idempotent, and panics if a different committee already exists for
  that epoch (COMS:72-85). See RCF R4.18 for the second insertion, from `reconfigure`.

Because the final checkpoint carries the next committee, state sync can verify checkpoints of
epoch `e+1` before the local node has reconfigured (RCF R4.14).

**R3.2 [E] Summary checks (`verify_checkpoint_with_committee`, SL:161-218).** Given the previous
verified summary `current` and a candidate:

1. `candidate.seq == current.seq + 1`. This is asserted: a violation panics.
2. `candidate.previous_digest == Some(current.digest())`. This is the hash chain.
3. `candidate.epoch` is either `current.epoch` or `current.epoch + 1`.
4. If `candidate.epoch == current.epoch + 1`, then `current` must be final
   (`next_epoch_committee().is_some()`).
5. `verify_authority_signatures(committee)`: `summary.epoch == auth_sig.epoch`, and the
   strong-quorum BLS check (MCK:514-522). The check (CRY:1296-1377) requires:
   - `sig_info.epoch == committee.epoch`;
   - every bit in `signers_map` is an index into the committee's sorted voting-rights list, with
     nonzero stake (duplicate bits are skipped);
   - the summed stake is ≥ `QUORUM_THRESHOLD` = 6,667 (COM:44);
   - the aggregate signature verifies over `Intent::sui_app(IntentScope::CheckpointSummary)` plus
     the BCS of the `CheckpointSummary`, with the epoch.

The genesis summary (sequence 0) is never verified this way. It comes with the genesis blob
(`insert_genesis_checkpoint`, CP:431-470). Pinned checkpoints skip verification (R2.6).

**R3.3 [E] The chain is anchored at the highest verified summary.** Verification always starts
from `highest_verified`. It never re-verifies earlier summaries, and never accepts a summary that
does not extend it. Because of the hash chain (R3.2 step 2), a node that has verified up to `n`
can only ever accept the unique certified continuation.

**R3.4 [E/I] The executor does not verify summaries again.** It trusts everything at or below
`highest_synced` (CEU:23-73). On validators, checkpoints from the aggregator enter as
`VerifiedCheckpoint::new_unchecked` (CPO:212), because the aggregator built the certificate itself.

### 3.2 Contents

**R3.5 [E] Contents checks (`VersionedFullCheckpointContents::verify_digests`, MCK:925-944).**

1. The `CheckpointContents` digest, rebuilt from the full contents, must equal the summary's
   `content_digest`. It is rebuilt from:
   - each transaction's `ExecutionDigests`, which are computed **from the effects**:
     `(effects.transaction_digest(), effects.digest())` (BT:1087-1089, FX:149-154);
   - the pinned user signatures. In V2 each signature is paired with its address-alias version
     (MCK:996-1009, MCK:1060-1066).
2. For each entry, `effects.transaction_digest() == transaction.digest()`.

The transaction digest covers only the `TransactionData` (TX:4274-4281). So the check binds
transaction data to effects, and effects to the certified summary.

**R3.6 [E/I] What is not verified.**

- **User signatures are never verified.** Not by state sync (`VerifiedCheckpointContents::
  new_unchecked`, SS:1911, MCK:1130-1149), not by the archive path (SSW:37), and not by the
  executor (`VerifiedTransaction::new_unchecked`, CE:839). The pinned `user_signatures` are bound
  by the contents digest. The signatures inside each `Transaction` envelope are not bound at all,
  and are not checked against the pinned ones. Both are irrelevant to execution results, because
  execution reads only `TransactionData`.
- Effects are not checked for internal consistency. They are trusted because the certified
  contents digest covers their digest, and execution recomputes them (R4.10).
- Events, objects and `unchanged_loaded_runtime_objects` are not transferred. Execution
  regenerates them; the events digest is part of the effects.
- `checkpoint_commitments` (the checkpoint artifacts digest) and the epoch's `epoch_commitments`
  (root state hash) in the summary are **not compared** with locally computed values, anywhere on
  the synced path. Searching `crates/sui-core/src` for `epoch_commitments`,
  `checkpoint_artifacts_digest()` and `end_of_epoch_data` finds only producers. The per-transaction
  effects digest is the only fork check on this path (R4.10).

### 3.3 What is stored, and the watermarks

**R3.7 [E/I] Storage of synced data.**

- A summary is written to `certified_checkpoints[seq]` and `checkpoint_by_digest`. If it is final,
  `epoch_last_checkpoint_map[epoch]` is also written (CP:823-858, CP:863-869). Then
  `HighestVerified` is set. For a final summary, the next committee is written first (R3.1).
- Full contents are written by `insert_checkpoint_contents` (STOR:402-413), in this order:
  1. the transactions and effects go into the perpetual `transactions` and `effects` tables, and
     into the cache. They are **not** marked as executed: no `executed_effects` entry is written
     (WBC:2552-2580, AST:905-924);
  2. one batch writes `checkpoint_sequence_by_contents_digest`, `full_checkpoint_content_v2[seq]`
     and `checkpoint_content[digest]` (CP:1023-1048).

  Then `HighestSynced` is set (CP:893-905), which wakes the executor.
- `full_checkpoint_content_v2` and the digest→sequence mapping are deleted 5000 checkpoints after
  execution (CE:1087-1114).

**R3.8 [E] Watermarks.** All three live in `CheckpointStoreTables::watermarks`, keyed by
`CheckpointWatermark` (CP:206-208, CP:1335-1344), with value `(seq, digest)`.

- `HighestVerified`: every summary up to it is verified and stored. It only increases
  (CP:871-891). It moves through state sync (`insert_checkpoint`), through
  `handle_checkpoint_from_consensus`, and through the archive path. The aggregator's
  `insert_certified_checkpoint` deliberately does **not** move it (CP:818-822).
- `HighestSynced`: every summary and contents up to it is stored. It is the server's advertised
  height (R1.9) and the executor's input (CEU:76-102). `RocksDbStore` keeps an in-memory copy and
  ignores regressions (STOR:372-385).
- `HighestExecuted`: every checkpoint up to it is executed and committed. It moves strictly by +1
  (CP:959-984, CE:1068-1080), and is the durable "executed" marker (RCF R3.4).
- Invariant: executed ≤ synced ≤ verified. The executor never runs ahead of synced.

**R3.9 [I] `HighestPruned`** is described in R1.8.

### 3.4 A hazard in the reference

**R3.10 [E] Anchovy must reject, not panic, on summaries with a bad epoch.** `verify_checkpoint`
looks up the committee for `checkpoint.epoch()` **before** the epoch checks of R3.2, and
**panics** if the committee is missing ("BUG: should have committee…", SL:228-234). A summary that
claims epoch `e+1` while `current` is not final has no committee yet, and neither does one that
claims `e+2` or later. Such a summary is rejected by R3.2 steps 3-4 only after the lookup.

**(uncertain:** It appears that a peer can crash a syncing sui node by answering
`BySequenceNumber(next)` with such a summary, or by pushing one for `highest_verified+1`. No
signature check comes first.**)** Anchovy should run the epoch checks before the committee lookup.

---

## 4. The checkpoint executor's synced path

This section covers `execute_transactions_from_synced_checkpoint` (CE:632-665), which runs:

- for every checkpoint on a node that does not run consensus (`NodeRole::runs_consensus()` is true
  only for validators and consensus observers, NR:59-64);
- for the final checkpoint everywhere (RCF R3.2);
- on validators, for any non-final checkpoint with no locally computed summary (R5.1)
  (CE:388-400, CE:554-561).

The per-epoch structure, the final-checkpoint handling and the persistence order of the final
checkpoint are in RCF R3.1-R3.6.

### 4.1 Loading

**R4.1 [E/I] Loading a checkpoint (CE:792-909).** The executor loads `checkpoint_content[digest]`
(it panics if this is missing), then the full contents, in bulk from `full_checkpoint_content_v2`
or one by one from the cache.

- Each transaction is wrapped as `VerifiedExecutableTransaction::new_from_checkpoint(tx,
  checkpoint.epoch, seq)` (CE:838-842, CE:880).
- `executed_fx_digests` is read for every transaction (CE:847-849).
- `CheckpointTransactionData::new` (CE:96-148) computes, per transaction, the accumulator version
  it reads (R4.4).

### 4.2 Input versions from certified effects

**R4.2 [E] Owned, immutable, receiving and package inputs come from the transaction.**
`get_input_object_keys` (EPS:1963-2004) and `schedule_transaction` (ESI:206-249) wait for:

- `ImmOrOwnedMoveObject(ref)` at `ref.version`;
- receiving objects at their declared version;
- packages by ID, at any version;
- shared objects at their **assigned** version, keyed by `(id, initial_shared_version)` from the
  transaction's declared `shared_input_objects()`. A missing assignment panics (EPS:1984-1990).

**R4.3 [E] Consensus-object versions come from the certified effects
(`assign_versions_from_effects`, SOVM:343-441, called through EPS:2417-2431 from CE:954-962).**
For one transaction:

1. `initial_version_map` maps id → `initial_shared_version`, from
   `transaction_data().shared_input_objects()` (SOVM:369-374).
2. `accessed_versions` maps id → version, from `effects.accessed_consensus_objects()` (SOVM:377-381).
   In effects V2 (FX2:124-156), this includes:
   - every changed object whose input state was `Exist` with a consensus owner: `Mutate`, at its
     **input** version;
   - every `unchanged_consensus_objects` entry: `ReadOnlyRoot(version)`,
     `MutateConsensusStreamEnded(v)`, `ReadConsensusStreamEnded(v)`, `Cancelled(v)`;
   - excluding `PerEpochConfig`, which is never sequenced.
3. The assignment is `((id, initial_shared_version), version)` for each accessed id that is a
   declared shared input. Accessed ids that are not declared must be in
   `IMPLICITLY_READ_SYSTEM_OBJECTS`, which today is only the accumulator root `0xacc`
   (SUIT:166). They are dropped (SOVM:382-397).

Consequences:

- **Cancelled transactions** are reproduced by the special versions in their effects:
  `CANCELLED_READ`, `CONGESTED` and `RANDOMNESS_UNAVAILABLE` are `MAX+1..MAX+3` (BT:1503-1563).
  Execution turns them into `ExecutionCancelledDueToSharedObjectCongestion` or
  `ExecutionCancelledDueToRandomnessUnavailable` early errors (XP:101-116). The synced path never
  computes congestion or randomness availability. It copies the verdict from the effects.
- **Deleted consensus objects** (stream ended) come back as the `*StreamEnded` version, which
  produces the `InputObjectDeleted` early error (XP:94-99).
- The randomness state update, the consensus commit prologue (clock), and settlement transactions
  are treated exactly the same way (R4.9).

**R4.4 [E] The accumulator root version is *not* taken from the effects.** The comment at
SOVM:412-414 says so: effects do not yet reliably record it. `CheckpointTransactionData::new`
(CE:103-141) reconstructs it from the checkpoint itself:

- Each barrier settlement transaction is recognized as the one whose effects change `0xacc`. Its
  root **input** version becomes the accumulator version of every transaction from the previous
  barrier up to and including itself.
- The function asserts that the checkpoint either has no barrier, ends with one, or ends with a
  barrier followed by the end-of-epoch tx. In the last case the end-of-epoch tx gets `None`.

The assignment then carries `system_object_versions = {0xacc: (initial_shared_version from the
epoch start config, that version)}` (SOVM:415-427). If the effects do record a root version that
differs, it is a `debug_fatal` (SOVM:398-411).

This version drives:

- the coin-reservation rewrite (AUTH:2051-2062);
- the wait for the root to reach that version for coin-reservation transactions (ESI:219-232);
- the object-funds checks (AUTH:2097-2145);
- the mainnet gas-smash gate (XP:21-24).

**R4.5 [E] Address-balance (funds) withdrawals.**

- **On the consensus path**, a transaction with withdrawals goes through the address-funds
  withdraw scheduler, which decides sufficiency (ESI:350-434).
- **On the synced path** the scheduler is bypassed (`enqueue_transactions`, CE:988-989). If the
  certified effects status is `Failure { error: InsufficientFundsForWithdraw }`, the env is marked
  `with_insufficient_funds()` (CE:972-979). This produces the `InsufficientFundsForWithdraw` early
  error (XP:118-121). Otherwise the withdrawal is executed normally.
- So the sufficiency verdict, like cancellation, is **copied from the effects**. The in-memory
  scheduler is not told about settlements executed this way (`settle_address_funds` is not called).
  It detects the advanced on-chain version and returns `SkipSchedule` for stale requests later
  (AWP:285-327, AFS:100-106, AFS:211-231).
- **(uncertain:** If a transaction had several early errors, for example cancelled *and*
  insufficient, the effects show only one status. The synced path sets the funds flag only for the
  insufficient-funds status. Whether this can diverge depends on how the engine reports multiple
  early errors, which was not traced.**)**

**R4.6 [E/I] Object-funds withdrawals** depend on the accumulator version (R4.4) and on
historical reads at it.

- With `check_object_funds_withdraw_in_execution` (on in devnet at 137 and on non-mainnet at 138,
  PC:4768-4770, PC:4783-4785), the check happens inside execution.
- Otherwise, the post-execution `object_funds_checker` may return `RetryLater`, and the execution
  driver retries after the next barrier settlement (AUTH:2097-2115, ED:187-193).
- See `object_funds_checking.md` in the same directory as AFS.

**R4.7 [E] First touch of a consensus object initializes its next version (SOVM:353-366,
EPS:2176-2250).** Before assigning from effects, `get_or_init_versions` is called for every
declared shared input, plus the accumulator root. Under a per-object mutex, an object's next
version is initialized from the object store **before** anything in the epoch mutates it. This
keeps the consensus path's version assigner correct when the synced path touches an object first
(R5.2). On a node that does not run consensus it has no effect on results.

### 4.3 Scheduling and order

**R4.8 [E/I] Order within and across checkpoints.**

- **Within a checkpoint**, `schedule_transaction_execution` (CE:913-992) enqueues every
  unexecuted, non-end-of-epoch transaction at once, with:
  - its assigned versions;
  - `ExpectedEffectsDigest::Certified { digest, checkpoint_seq }`;
  - barrier dependencies.

  `BarrierDependencyBuilder` (ESI:48-87) runs over the checkpoint in contents order. A
  transaction with a `Mutable` access to an object depends on every earlier
  `NonExclusiveWrite` to it. The builder is local to one checkpoint.

  Execution order is then decided by input availability (R4.2), not by position. Each
  transaction runs when its exact input versions exist (ESI:257-326). Already-executed
  transactions are not enqueued; their executed digest is checked instead (R4.10).
- **Across checkpoints**, the executor processes up to `checkpoint_execution_max_concurrency`
  (default 4, NCFG:1331-1333) checkpoints concurrently. Pipeline stages are ordered by sequence
  number: the stage that loads and **enqueues** checkpoint `i+1` starts only after the same stage
  for `i` has finished (CEU:282-420, CE:319-344). So the version initialization of R4.7 happens
  in checkpoint order. Execution of transactions from different checkpoints may overlap. Each
  commit stage is ordered (R4.13).
- **The final checkpoint** waits for `seq-1` to be executed. Its end-of-epoch tx runs last
  (RCF R3.3).

**R4.9 [E] System transactions on this path.** The consensus commit prologue (all versions), the
randomness state update, the authenticator state update, and accumulator settlement and barrier
transactions (`ProgrammableSystemTransaction`) are ordinary entries in the contents. Their full
`TransactionData` is shipped in the full contents. They are executed like user transactions, with
versions from effects (R4.3).

- No local state decides their content. In particular, the randomness bytes come from the
  shipped `RandomnessStateUpdate`. This path needs no DKG and no signatures (see RND §5).
- The end-of-epoch tx is skipped by the scheduler (CE:951-952). It is executed by
  `execute_change_epoch_tx` (CE:996-1064) in the final checkpoint only (RCF R3.3). Its versions
  are also from effects, with no accumulator version.
- Validators notify the randomness reporter for each round in the checkpoint after it is
  finalized (CE:474-492). The rounds come from `version_specific_data` (CE:1148-1190). This only
  matters to a node that runs the beacon.

### 4.4 Expected effects, and mismatch

**R4.10 [E] Every transaction on this path is checked against its certified effects digest.**

- If the transaction was already executed (by consensus, an earlier attempt, or a crash replay):
  `assert_not_forked` panics if the executed digest differs (CE:942-950, CEU:149-178).
  `try_execute_immediately` also asserts equality when it finds existing effects under the tx
  guard (AUTH:1557-1572).
- After a fresh execution: if `effects.digest() != expected`, the node dumps debug state, records
  `transaction_fork_detected_v2` with the checkpoint sequence number (CP:210-214), and calls
  **`fatal!`**, which halts the node (AUTH:2184-2245). There is no retry and no fallback. Recovery
  needs an operator override (`ForkRecoveryState`, AUTH:1544-1555) or a fixed binary.
- `ExecutionOutput::Fatal` from the pre-execution re-audit (`validity_check`,
  `check_certificate_input`, AUTH:2014-2029) is also `fatal!` (ED:184-186). Certified transactions
  are assumed to pass it.

**R4.11 [E] Inputs that must match the reference for effects to be byte-identical.** Beyond the
transaction and its assigned versions (R4.2-R4.5), execution reads:

- the epoch store's protocol config, epoch id, epoch start timestamp and reference gas price
  (AUTH:2014-2095);
- the exact object versions in the store, which equal the certified input versions only if all
  earlier transactions were applied identically;
- a node-local `certificate_deny_set` (AUTH:2036-2041). If it is non-empty, the node forks
  against certified effects. It is an operator escape hatch **[I]**.

Execution happens only in the epoch of the transaction's checkpoint. The execution lock rejects a
cross-epoch certificate (AUTH:1576-1593), and `enqueue_transactions` drops it (ESI:541-559).

### 4.5 Persistence

**R4.12 [E/I] Order of persistence for a synced, non-final checkpoint** (the final one adds the
steps in RCF R3.4). Execution writes outputs to the writeback cache, not to the database
(WBR §2). After all of the checkpoint's transactions have effects (CE:647-654), each step is
ordered across checkpoints by the pipeline:

1. Commit the post-processing index batches (CE:675-676, CE:714-734). On fullnodes, feed the
   congestion tracker (CE:680-686) **[I]**.
2. `insert_finalized_transactions`, which writes `executed_transactions_to_checkpoint` and
   notifies waiters (CE:688, CE:741-760, EPS:2104-2128).
3. `accumulate_checkpoint` over the effects, for the state hash (CE:693-699).
4. Build the DB batch of the checkpoint's outputs, with the highest-committed-checkpoint marker in
   the same batch (CE:415-427). Then commit the accumulator versions of unsettled object withdraws
   (CE:431-449), and **commit the transaction outputs atomically** (CE:451-466, WBR §3).
5. `handle_finalized_checkpoint`: drop signed-effects digests, and flush quarantined consensus
   output up to this checkpoint (CE:470-472, EPS:2057-2089).
6. Randomness reporter, subscription and RPC side effects (CE:474-500) **[I]**.
7. `accumulate_running_root` (CE:502-504).
8. Bump `HighestExecuted` and prune full contents from 5000 checkpoints back (CE:523,
   CE:1068-1128).

A crash anywhere before step 8 re-executes the checkpoint. Every earlier step must be idempotent
(RCF R3.4).

### 4.6 Summary of what is copied and what is recomputed

**R4.13 [E] Copied from the certified checkpoint:**

- transaction data;
- consensus-object input versions, including cancellations;
- the accumulator version, reconstructed from the settlement tx's effects;
- the funds-insufficiency verdict;
- the expected effects digest.

**Recomputed:** effects, objects, events, and the state hash.

**Never checked against the certificate:** the state hash, the root state hash, and the artifacts
digest (R3.6).

---

## 5. Mixed operation on a validator

**R5.1 [E] Which path each checkpoint takes on a validator.** Validators run consensus, so for a
non-final checkpoint `execute_checkpoint` calls `verify_locally_built_checkpoint` (CE:388-400):

- If `locally_computed_checkpoints[seq]` exists, the executor asserts that the certified summary
  matches it (`assert_checkpoint_not_forked`, CEU:180-252; it is fatal on a header or contents
  difference). It reuses the builder's state hash and executes nothing (CE:538-629). The
  transactions were already executed through consensus.
- If no local summary exists yet, it **falls back to the synced path** (CE:554-561, metric
  `checkpoint_executor_validator_sync_fallback_path`). This is the case when state sync is ahead
  of the local builder: after a restart, while catching up, or when consensus for the epoch is
  gone.
- The final checkpoint always takes the synced path (RCF R3.2).

So a sui validator **can and does execute synced checkpoints ahead of its own consensus**. The
choice is made per checkpoint, at the moment the executor reaches it.

**R5.2 [E] How races between the two paths are avoided.** Both paths may run on the same
transactions concurrently.

- **One execution per transaction.** `acquire_tx_guard` serializes executions of the same digest.
  The second execution finds the effects and returns them, asserting the expected digest
  (AUTH:1557-1572). The scheduler skips transactions that already have executed effects
  (ESI:560-574), and a pending transaction stops waiting once it has been executed
  (ESI:319-324).
- **Identical results.** Both paths assign the same versions: consensus deterministically, the
  synced path from certified effects. Version initialization at first touch is shared and
  mutex-protected (R4.7). So whichever path runs a transaction first, the outputs are identical,
  and the other path's expected-digest check passes.
- **Builder versus synced checkpoint.** When the builder later constructs a checkpoint that was
  already certified and executed, it compares its summary with the certified one
  (`check_for_checkpoint_fork`, CP:1867-1875). The builder reads effects that already exist.
- **Aggregator.** It discards signatures for sequence numbers already certified through state
  sync (CP:2416-2434). `handle_checkpoint_from_consensus` ignores checkpoints at or below
  `highest_verified` (SS:857-865).
- **Final checkpoint.** If state sync delivers the certified final checkpoint first, the builder
  stops when it finds the change-epoch tx already executed (RCF R2.6).
- **Address funds.** The scheduler copes with settlements executed by the synced path (R4.5).

**R5.3 [E] A lagging validator finishes the epoch through state sync.** Suppose a quorum has
reconfigured and stopped serving epoch N's consensus. The lagging validator's consensus cannot
progress, but:

1. its state sync keeps polling peers, which now report heights in N+1;
2. it fetches and verifies the remaining epoch-N summaries, and the final one;
3. it inserts committee N+1 (R3.1);
4. its executor runs the missing checkpoints on the synced fallback path (R5.1), then the final
   checkpoint (RCF R3.2-R3.5);
5. reconfiguration follows (RCF R2.6, R2.7, R3.7).

This works only if some connected peer serves state sync for those checkpoints (R2.12).

**R5.4 [E/I] Execution-time consistency inside one validator.** In `verify_locally_built_checkpoint`,
the presence check of the local summary and the builder are not synchronized. A checkpoint may be
built just after the executor chose the fallback path. That is harmless, by R5.2. Anchovy may use
any rule for choosing a path, provided that (a) every transaction is executed at most once, with
the certified effects, and (b) its consensus-path version assignment is never initialized from an
object state that the synced path has already advanced.

---

## 6. What anchovy must do for each scenario

**(i) Serve fullnodes and lagging sui validators.**

- Register all StateSync routes with the BCS+snappy codec: R1.1-R1.4.
- Serve the summary, contents-v2 and availability semantics: R1.5-R1.9. V1 contents may answer
  NotFound (R1.7).
- Behave as peers expect: R2.13, including the genesis summary from the first connection (R2.1)
  and pushing newly certified checkpoints (R2.4).
- Keep the store invariants behind the advertised watermarks: R3.7-R3.8, plus the invariant at
  SS:111-124 for checkpoints it built.
- Optional limits: R1.10-R1.11.
- To be a useful peer for sui's polling, it also needs to be in sui's peer table. That requires
  only the genesis probe to succeed (R2.1, R2.12). Discovery is out of scope **(uncertain** whether
  sui nodes keep a connection to a peer whose `Discovery` routes answer NotFound**)**.

**(ii) Catch up the remainder of an epoch whose consensus is gone.**

- Be a state-sync client: R2.1-R2.3 (find peers, chain check, heights), R2.6-R2.7 (summary and
  contents download), R2.10 (drop bad peers). Archive fallback is optional (R2.11).
- Verify: R3.1-R3.5, and reject rather than panic on bad epochs (R3.10). Store and track
  watermarks: R3.7-R3.8.
- Execute on the synced path: R4.1-R4.12. Anchovy's executor must take versions from effects, the
  accumulator version from the checkpoint's barrier, and the funds and cancellation verdicts from
  effects, and must check every effects digest.
- Mixed operation with its own consensus output for the same epoch: R5.1-R5.2 and R5.4,
  especially first-touch version initialization (R4.7).
- Finish the epoch: RCF R3.1-R3.5 and R4.x.

**(iii) Run as a fullnode.**

- Everything in (ii), with every checkpoint on the synced path (R4.x; CE:392-396).
- Everything in (i), so that other fullnodes can sync from it.
- Reconfiguration without consensus: RCF R4.19 (leaving or joining the committee) and RCF R4.14.
- Committee hand-off purely from `end_of_epoch_data`: R3.1.

---

## 7. Open questions and uncertainties

1. **R3.10** Whether the committee-lookup panic in `verify_checkpoint` is reachable by a peer.
   From the code it appears to be, through both the summary fetch and the push cache.
2. **R1.11** Whether the configurable contents limits are meant to apply to
   `GetCheckpointContentsV2`. As written, they attach only to the v1 route.
3. **R2.9** The exact membership of discovery's `chain_peers`. If it is the validator set,
   validators are never disconnected for state-sync failures.
4. **R4.5** Behaviour when a transaction has more than one early error and only one is visible in
   the effects status.
5. **R4.6** The `RetryLater` path of the post-execution object-funds checker on the synced path
   (mainnet and testnet at protocol 138), and the conditions under which it is retried.
6. **R3.6** No component compares the computed root state hash or checkpoint artifacts digest with
   the certified commitments. Anchovy may choose to add the check.
