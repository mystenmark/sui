// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Detecting a fork: each certified checkpoint against the one this
//! validator built.

use std::sync::Arc;

use messages::arena::BumpAlloc;
use messages::checkpoint::CheckpointSummary;
use messages::fast::Bump;
use messages::reader::Reader;
use validator::pipeline::items::ForkCheck;
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::Availability;

/// The fork waiter's availability check: the checkpoint this validator
/// built, from the checkpoint store, or, once pruned, as committed.
pub struct LocalCheckpoints {
    pub checkpoints: Arc<CheckpointStore>,
    pub store: Arc<store::Store>,
}

impl Availability<CheckpointBuilt, Arc<BuiltCheckpoint>> for LocalCheckpoints {
    fn check(&mut self, keys: &[CheckpointBuilt], found: &mut Vec<Option<Arc<BuiltCheckpoint>>>) {
        found.clear();
        found.extend(keys.iter().map(|&CheckpointBuilt(seq)| {
            self.checkpoints.built(seq).or_else(|| {
                let committed = self.store.checkpoint(seq).expect("the store reads")?;
                Some(Arc::new(BuiltCheckpoint::decode(&committed)))
            })
        }));
    }
}

/// The fork waiter's sink: each certified checkpoint must be the one this
/// validator built, its summary byte for byte.
///
/// # Panics
/// On a fork: carrying on would execute and serve a history the network
/// did not certify.
pub fn check_forks(ready: Vec<ForkCheck>) {
    // A summary allocates nothing as it parses.
    let bump = Bump::with_capacity(0);
    for check in ready {
        let seq = check.certified.seq;
        let local = check
            .local
            .expect("released once the local checkpoint resolved");
        let mut r = Reader::new(&check.certified.summary);
        let certified = CheckpointSummary::parse(&mut r, &mut BumpAlloc(&bump))
            .expect("a certified summary parses");
        assert!(
            certified.bytes == local.summary.as_slice(),
            "fork: certified checkpoint {seq} is not the one this validator built"
        );
    }
}
