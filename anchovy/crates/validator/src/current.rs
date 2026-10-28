// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The running epoch, as perpetual components reach it: absent while the
//! node reconfigures, so they wait for the next one.

use std::sync::Arc;

use messages::transaction::DigestPending;
use tokio::sync::watch;
use workqueue::Queue;

use crate::epoch::EpochState;
use crate::processors;
use crate::randomness::BeaconInput;
use crate::server::Consensus;

/// What perpetual components use of a running epoch. Dropped with it: its
/// queues close once the epoch's workers stop.
pub struct EpochHandle {
    pub epoch: Arc<EpochState>,
    /// Submissions, into the epoch's processors.
    pub transactions: Queue<processors::Request<DigestPending>>,
    /// Submission to consensus and waiting for effects.
    pub consensus: Consensus,
    /// The random beacon's signature collector, and each member's network
    /// key, by committee index: who may push it signatures.
    pub beacon: Queue<BeaconInput>,
    pub network_keys: Vec<[u8; 32]>,
}

/// The running epoch, if any. Cloned handles see the same epoch.
#[derive(Clone)]
pub struct CurrentEpoch {
    sender: Arc<watch::Sender<Option<Arc<EpochHandle>>>>,
}

impl CurrentEpoch {
    /// No epoch running yet.
    pub fn new() -> CurrentEpoch {
        CurrentEpoch {
            sender: Arc::new(watch::Sender::new(None)),
        }
    }

    pub fn set(&self, handle: Arc<EpochHandle>) {
        self.sender.send_replace(Some(handle));
    }

    /// No epoch runs until the next `set`.
    pub fn clear(&self) {
        self.sender.send_replace(None);
    }

    /// The running epoch, waiting while none runs.
    pub async fn get(&self) -> Arc<EpochHandle> {
        let mut receiver = self.sender.subscribe();
        let current = receiver
            .wait_for(Option::is_some)
            .await
            .expect("the sender lives as long as self");
        current.clone().expect("waited for an epoch")
    }

    /// The running epoch, if any, without waiting.
    pub fn now(&self) -> Option<Arc<EpochHandle>> {
        self.sender.borrow().clone()
    }

    /// Resolves once `handle`'s epoch is no longer the running one.
    pub async fn ended(&self, handle: &Arc<EpochHandle>) {
        let mut receiver = self.sender.subscribe();
        let _ = receiver
            .wait_for(|current| !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, handle)))
            .await;
    }
}

impl Default for CurrentEpoch {
    fn default() -> CurrentEpoch {
        CurrentEpoch::new()
    }
}
