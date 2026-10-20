// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Committing certified checkpoints to the database
//! (`docs/writeback_cache_design.md`, Commit path).

use std::sync::Arc;

use messages::fast::Bump;
use tokio::sync::watch;
use validator::epoch_store::CommittedCheckpoints;
use validator::pipeline::backpressure::Backpressure;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::{CheckpointStore, ChunkTable, GenerationTable};
use workqueue::Processor;

/// Commits each certified checkpoint's transactions from the writeback
/// cache to the database, with its record, certificate and the committed
/// watermark, in one batch. Then writes the epoch store's state that waited
/// for the checkpoint: the generations its transactions produced, and the
/// consensus output it covers. In that order, as the reference's
/// `execute_checkpoint`: a crash between them leaves the epoch store behind
/// the object store, which a replay catches up; the other order could leave
/// it claiming outputs a crash lost. Once the epoch's last checkpoint has
/// committed, says so: the epoch is over. The start of a `CheckpointExecutor`.
///
/// Inward: `CheckpointAggregator`: `Vec<CertifiedCheckpoint>`, in sequence
/// order.
///
/// Reads: the checkpoint store. Prunes: the chunk table. Writes: the object
/// store, as its only committer; the generation table's and the consensus
/// output's committed state; backpressure's committed watermark, which
/// releases the commit answers it withheld.
pub struct CheckpointCommitter {
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    chunks: Arc<ChunkTable>,
    generations: Arc<GenerationTable>,
    consensus_output: CommittedCheckpoints,
    backpressure: Arc<Backpressure>,
    /// The epoch's last checkpoint, once committed.
    ended: watch::Sender<Option<u64>>,
    bump: Bump,
}

impl CheckpointCommitter {
    pub fn new(
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        chunks: Arc<ChunkTable>,
        generations: Arc<GenerationTable>,
        consensus_output: CommittedCheckpoints,
        backpressure: Arc<Backpressure>,
        ended: watch::Sender<Option<u64>>,
    ) -> CheckpointCommitter {
        CheckpointCommitter {
            store,
            checkpoints,
            chunks,
            generations,
            consensus_output,
            backpressure,
            ended,
            bump: Bump::with_capacity(super::ARENA),
        }
    }

    /// # Panics
    /// If this validator did not build `certified`, its contents do not
    /// parse, or the store's commit panics.
    fn commit(&mut self, certified: &CertifiedCheckpoint) {
        let seq = certified.seq;
        let built = self
            .checkpoints
            .built(seq)
            .expect("a checkpoint is certified only once built here");
        let transactions = crate::transactions(&built.contents, &mut self.bump);
        self.store.commit_checkpoint(
            seq,
            &transactions,
            built.encode(),
            certified.summary.to_vec(),
        );
        let digests: Vec<[u8; 32]> = transactions.iter().map(|d| d.bytes).collect();
        self.generations.commit(&digests);
        self.consensus_output.committed_through(built.last_height);
        self.chunks.prune_through(built.last_height);
        self.checkpoints.committed(seq);
        self.backpressure.committed(seq, built.last_height);
        if built.root_state.is_some() {
            self.ended.send_replace(Some(seq));
        }
    }
}

impl Processor for CheckpointCommitter {
    type Input = Vec<CertifiedCheckpoint>;
    type Output = ();

    fn process(&mut self, certified: Vec<CertifiedCheckpoint>) -> Option<()> {
        for checkpoint in &certified {
            self.commit(checkpoint);
        }
        None
    }
}
