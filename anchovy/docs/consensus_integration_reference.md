# How sui wires consensus-core

Paths relative to `/Users/marklogan/projects/mlogan-anchovy`. `cm`
`crates/sui-core/src/consensus_manager/mod.rs`; `ch`
`crates/sui-core/src/consensus_handler.rs`; `an`
`consensus/core/src/authority_node.rs`.

## Committee and keys

`EpochStartSystemStateV1::get_consensus_committee`
(`crates/sui-types/src/sui_system_state/epoch_start_sui_system_state.rs:182-220`),
per active validator:

| `consensus_config::Authority` | from | type |
|---|---|---|
| `stake` | `voting_power` | u64, about 10,000 in all |
| `address` | `narwhal_primary_address` | Multiaddr |
| `hostname` | `hostname` | for metrics and logs |
| `authority_name` | `protocol_pubkey` bytes (BLS) | identity only |
| `protocol_key` | `narwhal_worker_pubkey` | Ed25519: signs blocks |
| `network_key` | `narwhal_network_pubkey` | Ed25519: TLS, peer identity |

Authorities are sorted by `authority_name`, so `AuthorityIndex(i)` is the
i-th member of the checkpoint committee; the verifier relies on it
(`consensus_validator.rs:76`). consensus-core finds its own index by its
protocol key, and panics if absent (`an:211-215`).

`NodeConfig` (`crates/sui-config/src/node.rs:62-69`): the protocol key (BLS,
not used by consensus-core), the worker key (`ProtocolKeyPair`, validators
only, `cm:236-240`), the network key (`NetworkKeyPair`, `cm:244`), the
account key (unused).

## ConsensusManager, per epoch

`start` (`cm:262-443`):
1. `ConsensusProtocolConfig` from `ProtocolConfig` (`cm:161-182`): protocol
   version, chain, transaction and block size limits, GC depth, slim block
   propagation, leaders per round, bad nodes threshold; transaction voting
   always on.
2. `Parameters`: the configured ones or defaults; `db_path =
   <consensus db>/<epoch>`; `listen_address_override` (`cm:335-339`).
3. A new metrics registry, `consensus` (`cm:341`).
4. `CommitConsumerArgs::new(replay_after, last_processed)`: `last_processed`
   is the handler's last flushed commit (0 at the epoch's start), and
   `replay_after` that less the commit rate window (`cm:345-352`).
5. The handler is spawned before the authority (`cm:356-363`).
6. The boot counter goes up once a previous authority handled a commit;
   at 0, consensus-core runs amnesia recovery (`cm:368-387`, `an:326-331`).
7. `ConsensusAuthority::start(Tonic, epoch start timestamp, committee,
   parameters, protocol config, worker keypair, network keypair,
   Clock::default(), verifier, no pool, commit consumer, registry, boot
   counter, None)` (`cm:389-405`).
8. The transaction client is published for submitters (`cm:406-408`).

`shutdown` (`cm:445-501`): clear the client, `authority.stop().await`
(the only reference), abort the handler. The node shuts consensus down
before reconfiguring, then prunes old consensus databases
(`crates/sui-node/src/lib.rs:2158`, `:2179`).

Submitters wait across epochs: the client poll waits for the next
authority's client (`mysticeti_adapter.rs:38-66`); a submission that meets
a stopped one fails with `ConsensusShuttingDown` and is retried
(`consensus_adapter.rs:730-772`).

## Verifier (`consensus/core/src/transaction.rs:533-557`)

Synchronous: `verify_and_vote_batch(&BlockRef, &[&[u8]]) ->
Result<Vec<TransactionIndex>, ValidationError>`. An error makes the block
invalid (its proposer equivocates). Called from `spawn_blocking` for peer,
fetched and commit-synced blocks, and inline (inside async code) for its
own blocks' sync and recovery re-votes (`block_verifier.rs:316-349`,
`transaction_vote_tracker.rs:121-128`, which panics on an error). So it must
block without needing, or tripping over, the tokio runtime.

## Commits (`consensus/core/src/commit_consumer.rs`, `commit.rs:343-378`)

An unbounded channel of `CommittedSubDag`: `leader`, `blocks` (sorted by
round, author; each with its transactions), `timestamp_ms`, `commit_ref`
(index from 1, contiguous; digest), `rejected_transactions_by_block`.
Commits are persisted before delivery; on start, those after
`replay_after` are sent again (`commit_observer.rs:143-268`).

The handler (`ch:3222-3279`) deserializes, then handles commits in order:
commits at or below `last_processed` only rebuild the commit interval
observer; after each, `CommitConsumerMonitor::set_highest_handled_commit`.
The monitor throttles commit sync (`commit_syncer.rs:236-243`) and lets
others wait for replay to finish (`cm:669-681`).

## Backpressure

The handler awaits `BackpressureManager` before each commit (`ch:1062-1066`):
backpressure while the writeback cache holds more than a threshold of
pending transactions, suppressed while every certified checkpoint has
executed, so checkpoint building cannot deadlock
(`crates/sui-core/src/authority/backpressure.rs`). The commit channel is
unbounded, so consensus keeps committing meanwhile; only commit sync slows.

## Submission (`consensus/core/src/transaction.rs:366-382`)

`submit(Vec<Vec<u8>>, Priority)` returns once included in a proposed block:
`(BlockRef, indices, oneshot<BlockStatus>)`, the status `Sequenced` or
`GarbageCollected`. Normal and high lanes (2,000 and 128), high first.
Errors: oversized transaction or bundle, shutting down.

## Network

Tonic only, TLS with the Ed25519 network keys, the committee's keys
allowed, server name `consensus_epoch_{epoch}`. It binds the committee
address's port on `0.0.0.0`, or the address itself if localhost
(`network/tonic_network.rs:859-870`).
