// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Closing the epoch: its last checkpoint, built whole
//! (`docs/reconfiguration_design.md`, The final checkpoint).

use std::sync::Arc;

use fastcrypto::hash::MultisetHash;
use messages::base::Digest;
use messages::fast::Bump;
use validator::consensus::commit::change_epoch_transaction;
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    CheckpointContentsBuilt, CheckpointEffects, Chunk, EffectsWaitItem, EndOfEpoch,
    EpochCloserInput, ExecutableTransaction, ExecutedEffects, LastOfEpoch, ScheduledEntry,
};
use validator::pipeline::keys::EffectsKey;
use validator::pipeline::tables::CheckpointStore;
use waiter::{Command, WaitBatch};
use workqueue::{Processor, Queue};

use crate::{contents_of, push, summary};

/// Passes each checkpoint on to `SummaryBuilder` once the previous one is
/// built, but the epoch's last, which it closes:
/// 1. Builds the change-epoch transaction: the epoch's gas totals through
///    this checkpoint (the previous one's are built), and this checkpoint's
///    timestamp as the next epoch's start.
/// 2. Hands it to the resolver with the shared accesses the commit handler
///    reserved, so it executes after everything else in the epoch, into the
///    writeback cache: built, not committed. It commits with the certified
///    checkpoint like any transaction.
/// 3. Once executed, builds the checkpoint again with it last, and hands
///    the whole checkpoint's effects to the hasher.
/// 4. Once the hasher has folded them, fills in the end-of-epoch data: the
///    next committee and protocol version from the system state the
///    transaction wrote, and the epoch's root.
///
/// Not yet: the end-of-epoch kinds beside `ChangeEpoch`, and protocol
/// upgrades (the next epoch keeps the protocol version).
///
/// Inward:
/// - built waiter, ready: `EpochCloserInput::Built`;
/// - effects waiter, ready: `EpochCloserInput::ChangeEpochExecuted`;
/// - `StateHasher`: `EpochCloserInput::Root`.
///
/// Outward:
/// - `VersionResolver`: the change-epoch transaction's `Chunk`;
/// - effects waiter, wait for: `ChangeEpochExecuted`, keyed by its
///   `EffectsKey`;
/// - `StateHasher`: the last checkpoint's `CheckpointEffects`;
/// - `SummaryBuilder`: `Vec<CheckpointContentsBuilt>`.
///
/// Reads: the checkpoint store, the object store.
pub struct EpochCloser {
    epoch: Arc<EpochParams>,
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    resolver: Queue<Chunk>,
    effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    hasher: Queue<CheckpointEffects>,
    summary: Queue<Vec<CheckpointContentsBuilt>>,
    closing: Option<Closing>,
    bump: Bump,
}

/// The last checkpoint, between the steps above.
struct Closing {
    built: CheckpointContentsBuilt,
    change_epoch: [u8; 32],
}

impl EpochCloser {
    pub fn new(
        epoch: Arc<EpochParams>,
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        resolver: Queue<Chunk>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        hasher: Queue<CheckpointEffects>,
        summary: Queue<Vec<CheckpointContentsBuilt>>,
    ) -> EpochCloser {
        EpochCloser {
            epoch,
            store,
            checkpoints,
            resolver,
            effects,
            hasher,
            summary,
            closing: None,
            bump: Bump::with_capacity(crate::ARENA),
        }
    }

    /// Step 1 and 2.
    fn close(&mut self, built: CheckpointContentsBuilt) {
        let seq = built.pending.seq;
        let close = built
            .pending
            .close
            .clone()
            .expect("only the epoch's last checkpoint is closed");
        let previous = self
            .checkpoints
            .built(seq - 1)
            .expect("a checkpoint is released once the previous one is built");
        let epoch = &self.epoch;
        let transaction = change_epoch_transaction(
            epoch.epoch,
            epoch.config.version.as_u64(),
            summary::rolling_gas(&built, Some(&previous), epoch.epoch),
            built.pending.timestamp_ms,
        );
        let digest = transaction.digest;
        push(
            &self.resolver,
            Chunk {
                version: close.version,
                root_version: close.root_version,
                round: close.round,
                height: *built
                    .pending
                    .heights
                    .last()
                    .expect("a checkpoint has chunks"),
                checkpoint_seq: seq,
                entries: vec![ScheduledEntry {
                    transaction: ExecutableTransaction::System(transaction),
                    shared: close.shared,
                    origin: None,
                }],
            },
            "resolver",
        );
        let mut wait = WaitBatch::with_capacity(1, 1);
        wait.push(EffectsWaitItem::ChangeEpochExecuted, [EffectsKey(digest)]);
        push(&self.effects, Command::Wait(wait), "effects waiter");
        self.closing = Some(Closing {
            built,
            change_epoch: digest,
        });
    }

    /// Step 3.
    fn executed(&mut self) {
        let change_epoch = self
            .closing
            .as_ref()
            .expect("closing the epoch")
            .change_epoch;
        let executed = self.change_epoch_effects(change_epoch);
        let closing = self.closing.as_mut().expect("closing the epoch");
        let Some(LastOfEpoch::Effects(mut effects)) = closing.built.last.take() else {
            unreachable!("the last checkpoint's effects are kept until it is built again")
        };
        effects.push(executed);
        let seq = closing.built.pending.seq;
        closing.built = contents_of(
            &self.store,
            &self.epoch,
            closing.built.pending.clone(),
            &effects,
            &mut self.bump,
        );
        push(
            &self.hasher,
            CheckpointEffects {
                seq,
                effects,
                last_of_epoch: true,
            },
            "hasher",
        );
    }

    /// The change-epoch transaction's executed effects.
    fn change_epoch_effects(&self, digest: [u8; 32]) -> ExecutedEffects {
        let effects_digest = self
            .store
            .executed_effects(&Digest::new(digest))
            .expect("the store reads")
            .expect("the change-epoch transaction executed");
        let effects = self
            .store
            .effects(&effects_digest)
            .expect("the store reads")
            .expect("executed effects are stored with their transaction");
        ExecutedEffects {
            digest,
            effects_digest: effects_digest.bytes,
            effects: effects.wire_bytes().into(),
        }
    }

    /// Step 4.
    fn rooted(&mut self, root_state: Vec<u8>) {
        let Closing { mut built, .. } = self.closing.take().expect("closing the epoch");
        let next = execution::system_state::read(&self.store)
            .expect("the change-epoch transaction wrote the system state");
        assert_eq!(
            next.epoch,
            self.epoch.epoch + 1,
            "the change-epoch transaction advanced the epoch"
        );
        let root_state_digest = self
            .epoch
            .config
            .commit_root_state_digest()
            .then(|| state_hasher::decode(&root_state).digest().digest);
        built.last = Some(LastOfEpoch::Closed(EndOfEpoch {
            next_committee: next.stakes(),
            next_protocol_version: next.protocol_version,
            root_state,
            root_state_digest,
        }));
        push(&self.summary, vec![built], "summary builder");
    }
}

impl Processor for EpochCloser {
    type Input = EpochCloserInput;
    type Output = ();

    fn process(&mut self, input: EpochCloserInput) -> Option<()> {
        match input {
            EpochCloserInput::Built(ready) => {
                let mut passed = Vec::with_capacity(ready.len());
                for built in ready {
                    if built.pending.close.is_some() {
                        self.close(built);
                    } else {
                        passed.push(built);
                    }
                }
                if !passed.is_empty() {
                    push(&self.summary, passed, "summary builder");
                }
            }
            EpochCloserInput::ChangeEpochExecuted => self.executed(),
            EpochCloserInput::Root(root) => self.rooted(root),
        }
        None
    }
}
