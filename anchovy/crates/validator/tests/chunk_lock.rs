// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The chunk lock: the commit stage claims its chunks, the synced scheduler
//! takes those past them and holds them until released.

use validator::pipeline::chunk_lock::{ChunkLock, Cut};

fn cut(seq: u64, height: u64) -> Cut {
    Cut { seq, height }
}

#[test]
fn the_synced_scheduler_takes_only_chunks_past_the_claimed() {
    let lock = ChunkLock::new(0, cut(0, 0));
    lock.claim(3, cut(1, 2));
    assert_eq!(lock.cut(), cut(1, 2));

    // Checkpoint 2 holds chunks 3 to 5: 3 is the commit stage's.
    assert_eq!(lock.take(3, 5), 3);
    assert!(!lock.held(3));
    assert!(lock.held(4) && lock.held(5));
    // Claimed later, they stay held until released.
    lock.claim(6, cut(2, 5));
    assert!(lock.held(4));
    lock.release(4);
    assert!(!lock.held(4) && lock.held(5));
    lock.release(5);
    assert!(!lock.held(5));
}
