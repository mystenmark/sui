// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Committing certified checkpoints to the database
//! (`docs/writeback_cache_design.md`, Commit path).

use std::collections::BTreeMap;
use std::sync::Arc;

use messages::fast::Bump;
use tokio::sync::watch;
use validator::pipeline::backpressure::Backpressure;
use validator::pipeline::items::{CertifiedCheckpoint, OutputWaits};
use validator::pipeline::keys::CheckpointProgress;
use validator::pipeline::tables::{CheckpointStore, GenerationTable, UnsettledWithdrawals};
use waiter::Command;
use workqueue::Processor;

/// Commits each certified checkpoint's transactions from the writeback
/// cache to the database, with its record, certificate and the committed
/// watermark, in one batch. Then writes the generations its transactions
/// produced, and tells the output waiter, which may then write the consensus
/// output it covers. In that order, as the reference's
/// `execute_checkpoint`: a crash between them leaves the epoch store behind
/// the object store, which a replay catches up; the other order could leave
/// it claiming outputs a crash lost. Once the epoch's last checkpoint has
/// committed, says so: the epoch is over. The start of a `CheckpointExecutor`.
///
/// Each checkpoint commits once, in sequence order, with whichever
/// certificate comes first: any quorum's certificate over the summary built
/// here is as good as another.
///
/// Inward: `CheckpointAggregator`, and the built waiter's fork check of
/// those fetched from peers: `Vec<CertifiedCheckpoint>`, each in sequence
/// order.
///
/// Outward: output waiter, notify: `CheckpointProgress::Committed(seq)`.
///
/// Reads: the checkpoint store. Writes: the object
/// store, as its only committer; the generation table's committed state;
/// the unsettled withdrawals, releasing those the checkpoint settled;
/// backpressure's committed watermark, which releases the commit answers it
/// withheld.
pub struct CheckpointCommitter {
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    generations: Arc<GenerationTable>,
    unsettled: Arc<UnsettledWithdrawals>,
    outputs: OutputWaits,
    backpressure: Arc<Backpressure>,
    /// The epoch's last checkpoint, once committed.
    ended: watch::Sender<Option<u64>>,
    /// Certified past the next to commit, by sequence number.
    waiting: BTreeMap<u64, CertifiedCheckpoint>,
}

impl CheckpointCommitter {
    pub fn new(
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        generations: Arc<GenerationTable>,
        unsettled: Arc<UnsettledWithdrawals>,
        outputs: OutputWaits,
        backpressure: Arc<Backpressure>,
        ended: watch::Sender<Option<u64>>,
    ) -> CheckpointCommitter {
        CheckpointCommitter {
            store,
            checkpoints,
            generations,
            unsettled,
            outputs,
            backpressure,
            ended,
            waiting: BTreeMap::new(),
        }
    }

    /// # Panics
    /// If this validator did not build `certified`, its contents do not
    /// parse, or the store's commit panics.
    fn commit(&mut self, certified: &CertifiedCheckpoint, bump: &Bump) {
        let seq = certified.seq;
        let built = self
            .checkpoints
            .built(seq)
            .expect("a checkpoint is certified only once built here");
        let transactions = crate::transactions(&built.contents, bump);
        self.store.commit_checkpoint(
            built.record(certified.summary.to_vec()),
            &transactions,
            bump,
        );
        self.generations.commit(&transactions, bump);
        self.unsettled.commit(&transactions);
        self.checkpoints.committed(seq);
        self.backpressure.committed(seq, built.last_height);
        if built.ends_epoch {
            self.ended.send_replace(Some(seq));
        }
        self.outputs.push(
            Command::notify([CheckpointProgress::Committed(seq)]),
            "output waiter",
        );
    }
}

impl Processor for CheckpointCommitter {
    type Input = Vec<CertifiedCheckpoint>;
    type Output = ();

    fn process(&mut self, certified: Vec<CertifiedCheckpoint>, bump: &mut Bump) -> Option<()> {
        let mut next = self.checkpoints.last_committed() + 1;
        for checkpoint in certified {
            if checkpoint.seq >= next {
                self.waiting.entry(checkpoint.seq).or_insert(checkpoint);
            }
        }
        while let Some(checkpoint) = self.waiting.remove(&next) {
            self.commit(&checkpoint, bump);
            bump.reset();
            next += 1;
        }
        None
    }
}
