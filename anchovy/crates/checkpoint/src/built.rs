// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The built waiter: what waits for this validator to build a checkpoint,
//! and detecting a fork once it has.

use std::sync::Arc;

use messages::arena::BumpAlloc;
use messages::checkpoint::CheckpointSummary;
use messages::fast::Bump;
use messages::reader::Reader;
use validator::pipeline::items::{
    BuiltWaitItem, CertifiedCheckpoint, CheckpointSignature, EpochCloserInput,
};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::Availability;
use workqueue::{Queue, Sink};

use crate::push;

/// The built waiter's availability check: the checkpoint this validator
/// built, from the checkpoint store (including the previous epoch's last).
pub struct BuiltAvailability {
    pub checkpoints: Arc<CheckpointStore>,
}

impl Availability<CheckpointBuilt, Arc<BuiltCheckpoint>> for BuiltAvailability {
    fn check(&mut self, keys: &[CheckpointBuilt], found: &mut Vec<Option<Arc<BuiltCheckpoint>>>) {
        found.clear();
        found.extend(
            keys.iter()
                .map(|&CheckpointBuilt(seq)| self.checkpoints.built(seq)),
        );
    }
}

/// Where the built waiter's ready items go, by kind: contents whose previous
/// checkpoint is built, to `EpochCloser`; signatures, to
/// `CheckpointAggregator`; certified checkpoints, to the fork check, here.
/// Runs on the built waiter's thread.
///
/// Inward: built waiter, ready: `Vec<BuiltWaitItem>`.
/// Outward: `EpochCloser`: `EpochCloserInput::Built`;
/// `CheckpointAggregator`: `Vec<CheckpointSignature>`.
pub struct BuiltRouter {
    pub epoch_closer: Queue<EpochCloserInput>,
    pub aggregator: Queue<Vec<CheckpointSignature>>,
}

impl Sink<Vec<BuiltWaitItem>> for BuiltRouter {
    fn send(&mut self, ready: Vec<BuiltWaitItem>) {
        let (mut next, mut signatures) = (Vec::new(), Vec::new());
        for item in ready {
            match item {
                BuiltWaitItem::Next(built) => next.push(built),
                BuiltWaitItem::Signature(signature) => signatures.push(signature),
                BuiltWaitItem::Certified { certified, local } => check_fork(
                    &certified,
                    &local.expect("released once the local checkpoint resolved"),
                ),
            }
        }
        if !next.is_empty() {
            push(
                &self.epoch_closer,
                EpochCloserInput::Built(next),
                "epoch closer",
            );
        }
        if !signatures.is_empty() {
            push(&self.aggregator, signatures, "aggregator");
        }
    }
}

/// A certified checkpoint must be the one this validator built, its summary
/// byte for byte.
///
/// # Panics
/// On a fork: carrying on would execute and serve a history the network
/// did not certify.
pub fn check_fork(certified: &CertifiedCheckpoint, local: &BuiltCheckpoint) {
    // A summary allocates nothing as it parses.
    let bump = Bump::with_capacity(0);
    let mut r = Reader::new(&certified.summary);
    let summary = CheckpointSummary::parse(&mut r, &mut BumpAlloc(&bump))
        .expect("a certified summary parses");
    assert!(
        summary.bytes == local.summary.as_slice(),
        "fork: certified checkpoint {} is not the one this validator built",
        certified.seq
    );
}
