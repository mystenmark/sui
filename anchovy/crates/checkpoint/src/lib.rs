// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Building checkpoints: contents as soon as their chunks settle, summaries
//! once the previous checkpoint is built (`docs/consensus_pipeline.md`,
//! Checkpointing).

mod built;
mod close;
mod commit;
pub mod contents;
mod key;
pub mod summary;

use std::sync::Arc;

use consensus::{Priority, TransactionClient};
use messages::Message;
use messages::arena::BumpAlloc;
use messages::base::Hashed;
use messages::checkpoint::{CheckpointContents, VersionedCheckpointContents};
use messages::fast::Bump;
use messages::reader::Reader;
use messages::transaction::SenderSignedData;
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    BuiltWaitItem, BuiltWaits, CheckpointContentsBuilt, CheckpointEffects, ExecutedEffects,
    LastOfEpoch, PendingCheckpoint,
};
use validator::pipeline::keys::{CheckpointBuilt, ChunkSettled};
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable};
use waiter::{Availability, Command, WaitBatch};
use workqueue::{Processor, Queue};

pub use built::{BuiltAvailability, BuiltRouter, check_fork};
pub use close::EpochCloser;
pub use commit::CheckpointCommitter;
pub use key::ProtocolKey;

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
/// - built waiter, wait for: `BuiltWaitItem::Next`, keyed by
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
    built: BuiltWaits,
}

impl ContentsBuilder {
    pub fn new(
        epoch: Arc<EpochParams>,
        chunks: Arc<ChunkTable>,
        store: Arc<store::Store>,
        hasher: Queue<CheckpointEffects>,
        built: BuiltWaits,
    ) -> ContentsBuilder {
        ContentsBuilder {
            epoch,
            chunks,
            store,
            hasher,
            built,
        }
    }

    /// The contents of `pending`, whose chunks have settled, and its
    /// effects in checkpoint order.
    /// `bump` is reset first.
    pub fn build(
        &mut self,
        pending: PendingCheckpoint,
        bump: &mut Bump,
    ) -> (CheckpointContentsBuilt, Vec<ExecutedEffects>) {
        let mut effects = Vec::new();
        for &height in &pending.heights {
            let chunk = self
                .chunks
                .get(height)
                .expect("a ready checkpoint's chunks are in the chunk table");
            effects.extend(chunk.effects.iter().cloned());
        }
        let built = contents_of(&self.store, &self.epoch, pending, &effects, bump);
        (built, effects)
    }
}

/// `pending`'s contents, of `effects` in checkpoint order. `bump` is reset
/// first: a checkpoint's temporaries can be large.
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
    bump.reset();
    let bump = &*bump;
    let mut stored = containers::Vec::with_capacity_in(effects.len(), bump);
    stored.extend(effects.iter().map(|e| {
        store
            .transaction(&e.digest)
            .expect("the store reads")
            .expect("an executed transaction is stored with its effects")
    }));
    let mut transactions = containers::Vec::with_capacity_in(stored.len(), bump);
    transactions.extend(
        stored
            .iter()
            .map(|m: &Message<SenderSignedData<'static>>| *m.get()),
    );
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

    fn process(&mut self, ready: Vec<PendingCheckpoint>, bump: &mut Bump) -> Option<()> {
        let mut waits = WaitBatch::with_capacity(ready.len(), ready.len());
        for pending in ready {
            let seq = pending.seq;
            let (mut built, effects) = self.build(pending, bump);
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
            waits.push(
                BuiltWaitItem::Next(built),
                seq.checked_sub(1).map(CheckpointBuilt),
            );
        }
        push(&self.built, Command::Wait(waits), "built waiter");
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
/// - built waiter, through `EpochCloser`: `Vec<CheckpointContentsBuilt>`.
///
/// Outward:
/// - built waiter, resolve: `CheckpointBuilt(seq)`, to the checkpoint built,
///   releasing `seq + 1`, its signatures and its certificate's fork check;
/// - consensus (black box): its checkpoint signature, at high priority.
///
/// Reads and writes: the checkpoint store.
pub struct SummaryBuilder {
    epoch: Arc<EpochParams>,
    checkpoints: Arc<CheckpointStore>,
    key: Arc<ProtocolKey>,
    built: BuiltWaits,
    consensus: TransactionClient,
}

impl SummaryBuilder {
    pub fn new(
        epoch: Arc<EpochParams>,
        checkpoints: Arc<CheckpointStore>,
        key: Arc<ProtocolKey>,
        built: BuiltWaits,
        consensus: TransactionClient,
    ) -> SummaryBuilder {
        SummaryBuilder {
            epoch,
            checkpoints,
            key,
            built,
            consensus,
        }
    }
}

impl Processor for SummaryBuilder {
    type Input = Vec<CheckpointContentsBuilt>;
    type Output = ();

    fn process(&mut self, ready: Vec<CheckpointContentsBuilt>, bump: &mut Bump) -> Option<()> {
        let bump = &*bump;
        let mut resolved = Vec::with_capacity(ready.len());
        let mut signatures = containers::Vec::with_capacity_in(ready.len(), bump);
        for built in ready {
            let seq = built.pending.seq;
            let previous = seq.checked_sub(1).map(|previous| {
                self.checkpoints
                    .built(previous)
                    .expect("a checkpoint is released once the previous one is built")
            });
            let summary = summary::build(&built, previous.as_deref(), &self.epoch, bump);
            signatures.push(summary::signature_transaction(
                &self.key,
                &summary.bytes,
                self.epoch.epoch,
                bump,
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
            let checkpoint = self.checkpoints.insert_built(checkpoint);
            resolved.push((CheckpointBuilt(seq), checkpoint));
        }
        // Every summary is in the checkpoint store: resolve.
        push(&self.built, Command::Resolve(resolved), "built waiter");
        for signature in signatures {
            // Where it is included is not needed: the built waiter waits for
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

/// The transactions of `contents` (`CheckpointContents` BCS), which this
/// validator built from the digests it computed, in order.
pub(crate) fn transactions<'b>(contents: &[u8], bump: &'b Bump) -> containers::Vec<'b, Hashed> {
    let mut r = Reader::new(contents);
    let parsed = CheckpointContents::parse(&mut r, &mut BumpAlloc(bump))
        .and_then(|c| r.finish().map(|()| c))
        .expect("built contents parse");
    let mut digests = containers::Vec::new_in(bump);
    match parsed.version {
        VersionedCheckpointContents::V1 { transactions, .. } => {
            digests.extend(transactions.iter().map(|t| Hashed::computed(t.transaction)));
        }
        VersionedCheckpointContents::V2(transactions) => {
            digests.extend(
                transactions
                    .iter()
                    .map(|t| Hashed::computed(t.digest.transaction)),
            );
        }
    }
    digests
}
