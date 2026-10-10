// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The decoded-transaction cache: a block's entries are taken once, and
//! eviction drops whole rounds.

use consensus::BlockRef;
use validator::consensus::cache::ConsensusTxCache;

fn block(round: u32, author: u32) -> BlockRef {
    BlockRef {
        round,
        author,
        digest: [round as u8 ^ author as u8; 32],
    }
}

#[test]
fn a_block_is_taken_once() {
    let cache = ConsensusTxCache::new();
    cache.insert(block(3, 1), vec![None, None]);
    assert_eq!(cache.take(&block(3, 1)).map(|e| e.len()), Some(2));
    assert!(cache.take(&block(3, 1)).is_none());
    assert!(cache.take(&block(3, 2)).is_none());
}

#[test]
fn eviction_drops_the_rounds_through_its_bound() {
    let cache = ConsensusTxCache::new();
    for round in 1..=5 {
        for author in 0..3 {
            cache.insert(block(round, author), vec![None]);
        }
    }
    cache.evict_through(3);
    assert_eq!(cache.len(), 6);
    assert!(cache.take(&block(3, 2)).is_none());
    assert!(cache.take(&block(4, 0)).is_some());
    cache.evict_through(u32::MAX);
    assert!(cache.is_empty());
}
