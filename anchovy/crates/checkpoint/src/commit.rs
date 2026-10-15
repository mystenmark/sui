// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Committing certified checkpoints to the database
//! (`docs/writeback_cache_design.md`, Commit path).

use std::sync::Arc;

use messages::arena::BumpAlloc;
use messages::base::Digest;
use messages::checkpoint::{CheckpointContents, VersionedCheckpointContents};
use messages::fast::Bump;
use messages::reader::Reader;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::{CheckpointStore, ChunkTable};
use workqueue::Processor;

/// Commits each certified checkpoint's transactions from the writeback
/// cache to the database, with its record, certificate and the committed
/// watermark, in one batch; then prunes what only uncommitted checkpoints
/// need. The start of a `CheckpointExecutor`.
///
/// Inward: `CheckpointAggregator`: `Vec<CertifiedCheckpoint>`, in sequence
/// order.
///
/// Reads and prunes: the checkpoint store, the chunk table. Writes: the
/// object store, as its only committer.
pub struct CheckpointCommitter {
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    chunks: Arc<ChunkTable>,
    bump: Bump,
}

impl CheckpointCommitter {
    pub fn new(
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        chunks: Arc<ChunkTable>,
    ) -> CheckpointCommitter {
        CheckpointCommitter {
            store,
            checkpoints,
            chunks,
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
        let transactions = transactions(&built.contents, &mut self.bump);
        self.store.commit_checkpoint(
            seq,
            &transactions,
            built.encode(),
            certified.summary.to_vec(),
        );
        // The next summary is built over this one.
        self.checkpoints.prune_below(seq);
        self.chunks.prune_through(built.last_height);
    }
}

/// The transactions of `contents` (`CheckpointContents` BCS), in order.
fn transactions(contents: &[u8], bump: &mut Bump) -> Vec<Digest> {
    bump.reset();
    let mut r = Reader::new(contents);
    let parsed = CheckpointContents::parse(&mut r, &mut BumpAlloc(bump))
        .and_then(|c| r.finish().map(|()| c))
        .expect("built contents parse");
    match parsed.version {
        VersionedCheckpointContents::V1 { transactions, .. } => {
            transactions.iter().map(|t| t.transaction).collect()
        }
        VersionedCheckpointContents::V2(transactions) => {
            transactions.iter().map(|t| t.digest.transaction).collect()
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
