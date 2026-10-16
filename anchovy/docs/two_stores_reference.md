# The reference's storage split

What a sui validator keeps on disk, who writes it and when, as of the sui
tree this repository sits in (`/Users/marklogan/projects/mlogan-anchovy`).
Abbreviations, relative to `crates/sui-core/src/`: `ast`
`authority/authority_store_tables.rs`, `as` `authority/authority_store.rs`,
`aes` `authority/authority_per_epoch_store.rs`, `cq`
`authority/consensus_quarantine.rs`, `cp` `checkpoints/mod.rs`, `ce`
`checkpoints/checkpoint_executor/mod.rs`, `wb`
`execution_cache/writeback_cache.rs`, `pr`
`authority/authority_store_pruner.rs`, `ch` `consensus_handler.rs`, `auth`
`authority.rs`; `node` is `sui-node/src/lib.rs`.

## On disk

| DB | Path | Lifetime |
|---|---|---|
| perpetual | `<db>/live/store/perpetual` (ast:174) | permanent |
| epoch | `<db>/live/store/epoch_<N>` (aes:135, aes:869) | one directory per epoch; old ones dropped by a pruner keeping the newest few |
| checkpoints | `<db>/live/checkpoints` (node:535) | permanent; contents pruned |
| committee | `<db>/live/epochs` (node:528) | permanent |
| indexes | `<db>/live/indexes` | full nodes only |
| consensus | consensus config's path | consensus's own |

The checkpoint and committee databases fold naturally into the perpetual
one, which also makes the reference's cross-database write orderings
single batches.

## Perpetual tables (ast:58-171)

Execution writes only to the writeback cache (wb:936). The checkpoint
executor writes a checkpoint's outputs as one batch, with
`highest_committed_checkpoint` (ce:415-427, as:722-810), then removes them
from the dirty set (wb:1098). A re-commit skips transactions no longer
dirty (wb:1067). At epoch end whatever is still dirty is dropped
(wb:1355).

| Table | Key → value | Notes |
|---|---|---|
| `objects` | (id, version) → object or tombstone | pruned by version from effects, never as "not latest" (ast:69) |
| `transactions`, `effects`, `executed_effects`, `events_2`, `unchanged_loaded_runtime_objects` | digest → … | the outputs batch; checkpoint pruner |
| `root_state_hash_by_epoch` | epoch → (last checkpoint, root) | written once at the epoch's last checkpoint (ce:511) |
| `epoch_start_configuration` | () → the current epoch's start | written at reconfiguration before the new epoch DB opens: the crash commit point (auth:3857, aes:367) |
| `executed_transaction_digests` | (epoch, digest) → () | last two epochs kept (pr:706) |
| `object_per_epoch_marker_table_v2` | (epoch, object key) → marker | epoch-scoped in practice: cleared at reconfiguration (as:183) |
| `highest_committed_checkpoint`, `pruned_checkpoint`, conservation singletons | () → … | |

## Checkpoint tables (cp:175-220), all perpetual

`checkpoint_content`, `certified_checkpoints`, `checkpoint_by_digest`,
`checkpoint_sequence_by_contents_digest`, `full_checkpoint_content_v2`,
`epoch_last_checkpoint_map`, watermarks (highest verified, synced,
executed, pruned), fork markers, and `locally_computed_checkpoints`, which
is epoch-scoped in practice (range-deleted at epoch end, ce:515). The
committee store is `epoch → committee`, written first at reconfiguration
(auth:3834).

## Epoch tables (aes:433-551)

Classes: **C** consensus output, written only by the consensus handler,
through one `ConsensusCommitOutput` per commit (cq:55, ch:949); **B**
checkpoint builder; **D** written directly; **X** written by execution or
the checkpoint executor.

| Table | Class | Holds |
|---|---|---|
| `consensus_message_processed` | C | processed transaction keys (dedup) |
| `last_consensus_stats_v2` | C, in every batch | the replay position: commit index, height, checkpoint seq |
| `next_shared_object_versions_v2` | C | (id, initial version) → next version |
| `owned_object_locked_transactions` | C | object ref → locking transaction (post-consensus only) |
| `deferred_transactions_with_aliases_v3`, randomness, DKG, JWK, congestion debts, execution time observations, `reconfig_state`, `end_of_publish` | C | |
| `builder_checkpoint_summary_v2` | B | seq → summary, height |
| `state_hash_by_checkpoint`, `running_root_accumulators` | D (builder's hasher) | rows past the last persisted builder summary are deleted at startup (cp:3014) |
| `authority_capabilities_v2` | D, but only from the handler task | |
| `effects_signatures`, `signed_effects_digests` | X | blocks signing two effects for one transaction |
| `executed_transactions_to_checkpoint`, `transaction_key_to_digest` | X | |

Pending checkpoints and checkpoint signatures are memory only, rebuilt by
replay (cq:415).

## The quarantine (cq:488-677)

"Holds the outputs of both consensus handler and checkpoint builder in
memory until they are proven not to have forked by a certified checkpoint"
(aes:326). Two producers only: the consensus handler and the checkpoint
builder. Readers look in the quarantine, then the DB (cq:755-986).

Flush, when a checkpoint has executed (ce:470, cq:595-676):
1. Every builder summary up to the executed checkpoint is written; note the
   highest height.
2. Of the queued commit outputs at or below that height, flush through the
   last one whose checkpoint queue was drained (`1ad56a6b81`: otherwise a
   restart loses roots queued but not yet in a checkpoint).

One batch per flush, `last_consensus_stats_v2` always in it. On restart
consensus replays from that position, and the handler's memory must be a
function of the replayed commits only (ch:212, ch:1024). So the DB never
holds consensus output from a forked local view, and a rebuilt checkpoint
with a different digest is a fork (cp:1816).

## Reconfiguration (auth:3834-3858, aes:1244)

Committee written; execution lock taken; dirty state dropped; conservation
checks; `epoch_start_configuration` written (the commit point); a new
`epoch_<N+1>` directory opened; the old store's handles released, later
uses fail with `EpochEnded`; old directories deleted by a pruner.

## Who writes what (bug history in brackets)

- Consensus handler: every C table, once per commit, through the
  quarantine. Each transaction advances shared versions exactly once
  (aes:458).
- Checkpoint builder: builder summaries, locally computed checkpoints, its
  contents. A re-derived sequence must keep its digest.
- Checkpoint executor: perpetual outputs, `HighestExecuted` (strictly +1),
  quarantine flushes.
- [`62091e97ff` pending checkpoints no longer persisted; `bca8a306df` no
  revert at epoch end; `22f9fc9781` owned locks post-consensus only;
  `94bfdb24ad` `highest_committed_checkpoint`, because `HighestExecuted`
  lagged the durable objects after a crash.]
