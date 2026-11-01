// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Writing consensus output once its checkpoints are built here and
//! committed (`docs/consensus_pipeline.md`, Checkpointing).

use std::sync::Arc;

use messages::fast::Bump;
use validator::epoch_store::OutputCommits;
use validator::pipeline::items::OutputThrough;
use validator::pipeline::keys::CheckpointProgress;
use validator::pipeline::tables::CheckpointStore;
use waiter::Availability;
use workqueue::Processor;

/// The output waiter's availability check: the checkpoint store's
/// watermarks.
pub struct OutputAvailability {
    pub checkpoints: Arc<CheckpointStore>,
}

impl Availability<CheckpointProgress> for OutputAvailability {
    fn check(&mut self, keys: &[CheckpointProgress], found: &mut Vec<Option<()>>) {
        found.clear();
        found.extend(keys.iter().map(|key| {
            let available = match *key {
                CheckpointProgress::BuiltHere(seq) => self.checkpoints.built_here() >= seq,
                CheckpointProgress::Committed(seq) => self.checkpoints.last_committed() >= seq,
            };
            available.then_some(())
        }));
    }
}

/// Writes the consensus output of each commit the output waiter releases,
/// and of every commit before it.
///
/// Both keys are needed. Committed: the epoch store may lag the object
/// store after a crash, never lead it. Built here: a certified checkpoint
/// can commit before consensus reaches its chunks, and output written then
/// would not be decided again after a crash, so the consensus path's checks
/// of its transactions, executed already, would never run.
///
/// Inward: output waiter, ready: `Vec<OutputThrough>`.
///
/// Writes: the consensus output.
pub struct OutputCommitter {
    output: OutputCommits,
}

impl OutputCommitter {
    pub fn new(output: OutputCommits) -> OutputCommitter {
        OutputCommitter { output }
    }
}

impl Processor for OutputCommitter {
    type Input = Vec<OutputThrough>;
    type Output = ();

    fn process(&mut self, ready: Vec<OutputThrough>, _: &mut Bump) -> Option<()> {
        if let Some(OutputThrough(n)) = ready.into_iter().max_by_key(|o| o.0) {
            self.output.write_through(n);
        }
        None
    }
}
