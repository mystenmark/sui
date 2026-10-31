// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The chunk lock: the commit stage claims its chunks, the synced scheduler
//! takes those past them, and neither runs a chunk the other holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use validator::pipeline::chunk_lock::{ChunkLock, Cut};

fn cut(seq: u64, height: u64) -> Cut {
    Cut { seq, height }
}

#[test]
fn the_synced_scheduler_takes_only_chunks_past_the_claimed() {
    let lock = ChunkLock::new(0, cut(0, 0));
    assert!(lock.claim(3, cut(1, 2)));
    assert_eq!(lock.cut(), cut(1, 2));

    // Checkpoint 2 holds chunks 3 to 5: 3 is the commit stage's.
    assert_eq!(lock.take(3, 5), 3);
    for height in [4, 5] {
        lock.release(height);
    }
    // Claims never wait for chunks released.
    assert!(lock.claim(6, cut(2, 5)));
}

/// A claim moves the commit stage's height at once, then waits for the
/// chunks the synced scheduler holds to be released.
#[test]
fn a_claim_waits_for_the_synced_chunks_it_covers() {
    let lock = Arc::new(ChunkLock::new(2, cut(1, 2)));
    assert_eq!(lock.take(3, 4), 2);
    let done = Arc::new(AtomicBool::new(false));
    let claimer = {
        let (lock, done) = (lock.clone(), done.clone());
        thread::spawn(move || {
            let ok = lock.claim(5, cut(2, 4));
            done.store(true, Ordering::SeqCst);
            ok
        })
    };
    while lock.cut() != cut(2, 4) {
        thread::yield_now();
    }
    // Claimed, though still waiting: chunk 5 is not the scheduler's.
    assert_eq!(lock.take(5, 5), 5);
    thread::sleep(Duration::from_millis(50));
    assert!(!done.load(Ordering::SeqCst));

    lock.release(3);
    thread::sleep(Duration::from_millis(50));
    assert!(!done.load(Ordering::SeqCst));
    lock.release(4);
    assert!(claimer.join().unwrap());
}

#[test]
fn a_claim_on_chunks_held_when_the_scheduler_stops_fails() {
    let lock = Arc::new(ChunkLock::new(0, cut(0, 0)));
    lock.take(1, 1);
    let claimer = {
        let lock = lock.clone();
        thread::spawn(move || lock.claim(1, cut(1, 1)))
    };
    thread::sleep(Duration::from_millis(50));
    lock.close();
    assert!(!claimer.join().unwrap());
    // Chunks it never held are claimed as ever.
    assert!(lock.claim(2, cut(1, 1)));
}
