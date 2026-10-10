// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transactions decoded while voting on a block, kept for when the block is
//! committed. Not in the reference, which decodes a block's transactions
//! when voting and again when handling the commit.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use ::consensus::BlockRef;

use crate::checks::VerifiedTransaction;

/// A block's decoded transactions, by index: a user transaction whose
/// validity and signatures were checked, or `None` for any other.
pub type BlockEntries = Vec<Option<VerifiedTransaction>>;

/// Decoded transactions by block. Voters insert a block's entries; the
/// commit handler takes them when the block is committed, and evicts the
/// blocks of rounds consensus no longer commits.
///
/// An entry is what decoding and verifying the block's bytes again would
/// give: a block reference names one block (its digest covers the block's
/// bytes), so a hit's transaction has the bytes the commit carries.
#[derive(Default)]
pub struct ConsensusTxCache {
    // Ordered by round first, so eviction splits the map.
    blocks: Mutex<BTreeMap<BlockRef, BlockEntries>>,
}

impl ConsensusTxCache {
    pub fn new() -> ConsensusTxCache {
        ConsensusTxCache::default()
    }

    /// Keeps `entries` for `block`, replacing any kept before: a block voted
    /// on again (as after a restart) gives the same entries.
    pub fn insert(&self, block: BlockRef, entries: BlockEntries) {
        self.lock().insert(block, entries);
    }

    /// Takes `block`'s entries, if kept: a block is committed once.
    pub fn take(&self, block: &BlockRef) -> Option<BlockEntries> {
        self.lock().remove(block)
    }

    /// Drops the entries of blocks in rounds up to and including `round`.
    pub fn evict_through(&self, round: u32) {
        let Some(first_kept) = round.checked_add(1) else {
            self.lock().clear();
            return;
        };
        let mut blocks = self.lock();
        let kept = blocks.split_off(&BlockRef {
            round: first_kept,
            author: 0,
            digest: [0; 32],
        });
        *blocks = kept;
    }

    /// The number of blocks kept.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<BlockRef, BlockEntries>> {
        // A panic mid-insert or mid-split leaves the map whole.
        self.blocks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
