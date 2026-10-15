// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Building checkpoints: contents as soon as their chunks settle, summaries
//! once the previous checkpoint is built (`docs/consensus_pipeline.md`,
//! Checkpointing).

mod close;
mod commit;
pub mod contents;
mod fork;
mod key;
pub mod summary;

use std::sync::Arc;

use consensus::{Priority, TransactionClient};
use messages::Message;
use messages::base::Digest;
use messages::fast::Bump;
use messages::transaction::SenderSignedData;
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    CheckpointContentsBuilt, CheckpointEffects, CheckpointSignature, ExecutedEffects, ForkCheck,
    LastOfEpoch, PendingCheckpoint,
};
use validator::pipeline::keys::{CheckpointBuilt, ChunkSettled, LocalSummary};
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable};
use waiter::{Availability, Command, WaitBatch};
use workqueue::{Processor, Queue};

pub use close::EpochCloser;
pub use commit::CheckpointCommitter;
pub use fork::{LocalCheckpoints, check_forks};
pub use key::ProtocolKey;

/// Arena for one checkpoint's parsed effects and contents; a larger
/// checkpoint spills into extra chunks, freed at the next reset.
const ARENA: usize = 1 << 20;

/// Pushes `item` to `queue`. Inside the pipeline a full queue is a sizing
/// bug, not load to shed; a closed one means the pipeline is shutting down.
fn push<T>(queue: &Queue<T>, item: T, name: &str) {
    if let Err(workqueue::PushError::Full(_)) = queue.try_push(item) {
        panic!("the {name} queue refused work");
    }
}

/// Builds everything of a checkpoint that needs only the checkpoint itself
/// (many checkpoints in flight): its contents (execution digests and user
/// signatures) and their digest, its gas cost summary, the artifacts
/// commitment and its randomness rounds, from the chunk table's ordered
/// effects and their transactions' signatures.
///
/// Checkpoint `seq` then waits for `CheckpointBuilt(seq - 1)`, checkpoint 0
/// for nothing. The first checkpoint of an epoch waits like any other: the
/// checkpoint store holds the previous epoch's last. The epoch's last
/// checkpoint is built without the change-epoch transaction, which
/// `EpochCloser` adds; its effects go there, not to the hasher.
///
/// Inward:
/// - checkpoint waiter, ready: `Vec<PendingCheckpoint>`.
///
/// Outward:
/// - `StateHasher`: `CheckpointEffects`, but the epoch's last checkpoint's;
/// - sequence waiter, wait for: `CheckpointContentsBuilt`, keyed by
///   `CheckpointBuilt(seq - 1)`.
///
/// Reads: the chunk table; the object store's transactions, for their
/// signatures (stored with their effects, so present for every settled
/// chunk).
pub struct ContentsBuilder {
    epoch: Arc<EpochParams>,
    chunks: Arc<ChunkTable>,
    store: Arc<store::Store>,
    hasher: Queue<CheckpointEffects>,
    sequence: Queue<Command<CheckpointBuilt, CheckpointContentsBuilt>>,
    bump: Bump,
}

impl ContentsBuilder {
    pub fn new(
        epoch: Arc<EpochParams>,
        chunks: Arc<ChunkTable>,
        store: Arc<store::Store>,
        hasher: Queue<CheckpointEffects>,
        sequence: Queue<Command<CheckpointBuilt, CheckpointContentsBuilt>>,
    ) -> ContentsBuilder {
        ContentsBuilder {
            epoch,
            chunks,
            store,
            hasher,
            sequence,
            bump: Bump::with_capacity(ARENA),
        }
    }

    /// The contents of `pending`, whose chunks have settled, and its
    /// effects in checkpoint order.
    pub fn build(
        &mut self,
        pending: PendingCheckpoint,
    ) -> (CheckpointContentsBuilt, Vec<ExecutedEffects>) {
        let mut effects = Vec::new();
        for &height in &pending.heights {
            let chunk = self
                .chunks
                .get(height)
                .expect("a ready checkpoint's chunks are in the chunk table");
            effects.extend(chunk.effects.iter().cloned());
        }
        let built = contents_of(&self.store, &self.epoch, pending, &effects, &mut self.bump);
        (built, effects)
    }
}

/// `pending`'s contents, of `effects` in checkpoint order.
///
/// # Panics
/// If the store cannot be read or lacks an executed transaction.
fn contents_of(
    store: &store::Store,
    epoch: &EpochParams,
    pending: PendingCheckpoint,
    effects: &[ExecutedEffects],
    bump: &mut Bump,
) -> CheckpointContentsBuilt {
    let stored: Vec<Message<SenderSignedData<'static>>> = effects
        .iter()
        .map(|e| {
            store
                .transaction(&Digest::new(e.digest))
                .expect("the store reads")
                .expect("an executed transaction is stored with its effects")
        })
        .collect();
    let transactions: Vec<SenderSignedData<'_>> = stored.iter().map(|m| *m.get()).collect();
    let contents = contents::build(&epoch.config, effects, &transactions, bump);
    CheckpointContentsBuilt {
        transaction_count: effects.len() as u64,
        pending,
        contents: contents.bytes,
        contents_digest: contents.digest,
        gas: contents.gas,
        artifacts_digest: contents.artifacts_digest,
        randomness_rounds: contents.randomness_rounds,
        last: None,
    }
}

impl Processor for ContentsBuilder {
    type Input = Vec<PendingCheckpoint>;
    type Output = ();

    fn process(&mut self, ready: Vec<PendingCheckpoint>) -> Option<()> {
        let mut waits = WaitBatch::with_capacity(ready.len(), ready.len());
        for pending in ready {
            let seq = pending.seq;
            let (mut built, effects) = self.build(pending);
            if built.pending.close.is_some() {
                // Built again, and hashed, with the change-epoch transaction.
                built.last = Some(LastOfEpoch::Effects(effects));
            } else {
                push(
                    &self.hasher,
                    CheckpointEffects {
                        seq,
                        effects,
                        last_of_epoch: false,
                    },
                    "hasher",
                );
            }
            waits.push(built, seq.checked_sub(1).map(CheckpointBuilt));
        }
        push(&self.sequence, Command::Wait(waits), "sequence waiter");
        Some(())
    }
}

/// Builds what of a checkpoint needs the previous one: the previous digest,
/// the network total, the epoch's rolling gas cost, then the summary; records
/// it, notifies, signs and submits the signature. The checkpoint reaches the
/// database only once certified, with its transactions
/// (`CheckpointCommitter`).
///
/// Inward:
/// - sequence waiter, ready: `Vec<CheckpointContentsBuilt>`.
///
/// Outward:
/// - sequence waiter, notify: `CheckpointBuilt(seq)`, releasing `seq + 1`;
/// - summary waiter, notify: `LocalSummary(seq)`;
/// - fork waiter, resolve: `CheckpointBuilt(seq)`, to the checkpoint built;
/// - consensus (black box): its checkpoint signature, at high priority.
///
/// Reads and writes: the checkpoint store.
pub struct SummaryBuilder {
    epoch: Arc<EpochParams>,
    /// Resolves each built checkpoint for the fork check, once wired.
    fork_waiter: Option<Queue<Command<CheckpointBuilt, ForkCheck, Arc<BuiltCheckpoint>>>>,
    checkpoints: Arc<CheckpointStore>,
    key: Arc<ProtocolKey>,
    sequence: Queue<Command<CheckpointBuilt, CheckpointContentsBuilt>>,
    summaries: Queue<Command<LocalSummary, CheckpointSignature>>,
    consensus: TransactionClient,
    bump: Bump,
}

impl SummaryBuilder {
    pub fn new(
        epoch: Arc<EpochParams>,
        checkpoints: Arc<CheckpointStore>,
        key: Arc<ProtocolKey>,
        sequence: Queue<Command<CheckpointBuilt, CheckpointContentsBuilt>>,
        summaries: Queue<Command<LocalSummary, CheckpointSignature>>,
        consensus: TransactionClient,
    ) -> SummaryBuilder {
        SummaryBuilder {
            epoch,
            fork_waiter: None,
            checkpoints,
            key,
            sequence,
            summaries,
            consensus,
            bump: Bump::with_capacity(4096),
        }
    }

    /// Resolves each built checkpoint for the fork check.
    #[must_use]
    pub fn with_fork_waiter(
        mut self,
        fork_waiter: Queue<Command<CheckpointBuilt, ForkCheck, Arc<BuiltCheckpoint>>>,
    ) -> SummaryBuilder {
        self.fork_waiter = Some(fork_waiter);
        self
    }
}

impl Processor for SummaryBuilder {
    type Input = Vec<CheckpointContentsBuilt>;
    type Output = ();

    fn process(&mut self, ready: Vec<CheckpointContentsBuilt>) -> Option<()> {
        let mut built_seqs = Vec::with_capacity(ready.len());
        let mut signatures = Vec::with_capacity(ready.len());
        for built in ready {
            let seq = built.pending.seq;
            let previous = seq.checked_sub(1).map(|previous| {
                self.checkpoints
                    .built(previous)
                    .expect("a checkpoint is released once the previous one is built")
            });
            let summary = summary::build(&built, previous.as_deref(), &self.epoch, &mut self.bump);
            signatures.push(summary::signature_transaction(
                &self.key,
                &summary.bytes,
                self.epoch.epoch,
            ));
            let checkpoint = BuiltCheckpoint {
                seq,
                summary: summary.bytes,
                digest: summary.digest,
                contents: built.contents.to_vec(),
                network_total_transactions: summary.network_total_transactions,
                epoch_rolling_gas: summary.epoch_rolling_gas,
                last_height: *built
                    .pending
                    .heights
                    .last()
                    .expect("a checkpoint has chunks"),
                timestamp_ms: built.pending.timestamp_ms,
                root_state: match &built.last {
                    Some(LastOfEpoch::Closed(end)) => Some(end.root_state.clone()),
                    _ => None,
                },
            };
            self.checkpoints.insert_built(checkpoint);
            built_seqs.push(seq);
        }
        // Every summary is in the checkpoint store: notify.
        if let Some(fork_waiter) = &self.fork_waiter {
            let resolved = built_seqs
                .iter()
                .map(|&seq| {
                    let built = self.checkpoints.built(seq).expect("just built");
                    (CheckpointBuilt(seq), built)
                })
                .collect();
            push(fork_waiter, Command::Resolve(resolved), "fork waiter");
        }
        push(
            &self.summaries,
            Command::notify(built_seqs.iter().copied().map(LocalSummary)),
            "summary waiter",
        );
        push(
            &self.sequence,
            Command::notify(built_seqs.into_iter().map(CheckpointBuilt)),
            "sequence waiter",
        );
        for signature in signatures {
            // Where it is included is not needed: the summary waiter waits for
            // whatever signatures consensus commits. Failing only as
            // consensus shuts down, when nothing is left to sign for.
            let _ = self
                .consensus
                .submit_blocking(vec![signature], Priority::High);
        }
        Some(())
    }
}

/// The checkpoint waiter's availability check: a chunk the chunk table
/// holds.
pub struct ChunkAvailability {
    pub chunks: Arc<ChunkTable>,
}

impl Availability<ChunkSettled> for ChunkAvailability {
    fn check(&mut self, keys: &[ChunkSettled], found: &mut Vec<Option<()>>) {
        found.clear();
        found.extend(keys.iter().map(|k| self.chunks.get(k.0).map(|_| ())));
    }
}

/// The sequence waiter's availability check: a checkpoint the checkpoint
/// store holds as built (including the previous epoch's last).
pub struct BuiltAvailability {
    pub checkpoints: Arc<CheckpointStore>,
}

impl Availability<CheckpointBuilt> for BuiltAvailability {
    fn check(&mut self, keys: &[CheckpointBuilt], found: &mut Vec<Option<()>>) {
        found.clear();
        found.extend(
            keys.iter()
                .map(|k| self.checkpoints.is_built(k.0).then_some(())),
        );
    }
}
