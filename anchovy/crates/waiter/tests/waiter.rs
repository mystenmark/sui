// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The waiter's rules one by one, then against a model over random
//! interleavings of waits, keys becoming visible, and their notifications.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use waiter::{Availability, WaitBatch, Waiter};

/// The keys a check was asked about.
type Asked = Rc<RefCell<Vec<u32>>>;

/// A waiter whose check asks `visible`, logging the keys it is asked about.
fn waiter(visible: &[u32]) -> (Waiter<u32, &'static str, impl Availability<u32>>, Asked) {
    let visible: HashSet<u32> = visible.iter().copied().collect();
    let asked = Rc::new(RefCell::new(Vec::new()));
    let log = asked.clone();
    let waiter = Waiter::new(move |key: &u32| {
        log.borrow_mut().push(*key);
        visible.contains(key)
    });
    (waiter, asked)
}

fn batch(items: &[(&'static str, &[u32])]) -> WaitBatch<u32, &'static str> {
    let mut batch = WaitBatch::new();
    for (item, keys) in items {
        batch.push(*item, keys.iter().copied());
    }
    batch
}

fn ready(waiter: &mut Waiter<u32, &'static str, impl Availability<u32>>) -> Vec<&'static str> {
    let mut out = Vec::new();
    waiter.get_ready(&mut out);
    out
}

#[test]
fn an_item_whose_keys_are_available_is_ready_at_once() {
    let (mut w, _) = waiter(&[1, 2]);
    w.wait_for(&mut batch(&[("a", &[1, 2]), ("b", &[])]));
    // `b` waits on nothing; `a`'s keys are found available by the check,
    // which runs after the batch is registered.
    assert_eq!(ready(&mut w), ["b", "a"]);
    assert_eq!(w.waiting(), 0);
    assert_eq!(w.pending_keys(), 0);
}

#[test]
fn an_item_is_ready_once_its_last_key_is_notified() {
    let (mut w, _) = waiter(&[1]);
    w.wait_for(&mut batch(&[("a", &[1, 2, 3])]));
    assert!(ready(&mut w).is_empty());
    w.notify(&[3]);
    assert!(ready(&mut w).is_empty());
    w.notify(&[2]);
    assert_eq!(ready(&mut w), ["a"]);
    assert_eq!(w.waiting(), 0);
}

#[test]
fn a_repeated_key_counts_once() {
    let (mut w, _) = waiter(&[]);
    w.wait_for(&mut batch(&[("a", &[5, 6, 5, 5])]));
    w.notify(&[5]);
    assert!(ready(&mut w).is_empty());
    w.notify(&[6]);
    assert_eq!(ready(&mut w), ["a"]);
}

#[test]
fn many_items_wait_on_one_key() {
    let (mut w, _) = waiter(&[]);
    w.wait_for(&mut batch(&[
        ("a", &[1]),
        ("b", &[1, 2]),
        ("c", &[1]),
        ("d", &[1]),
    ]));
    assert_eq!(w.pending_keys(), 2);
    w.notify(&[1]);
    assert_eq!(ready(&mut w), ["a", "c", "d"]);
    w.notify(&[2]);
    assert_eq!(ready(&mut w), ["b"]);
}

#[test]
fn a_key_nothing_waits_on_is_forgotten() {
    let (mut w, _) = waiter(&[]);
    w.notify(&[7, 8]);
    assert_eq!(w.pending_keys(), 0);
    // Without the check seeing it, a later wait waits.
    w.wait_for(&mut batch(&[("a", &[7])]));
    assert!(ready(&mut w).is_empty());
}

#[test]
fn a_batch_checks_its_untracked_keys_once_each() {
    let (mut w, asked) = waiter(&[1]);
    w.wait_for(&mut batch(&[
        ("a", &[1, 2]),
        ("b", &[2, 3]),
        ("c", &[3, 1]),
    ]));
    let mut keys = asked.borrow().clone();
    keys.sort_unstable();
    assert_eq!(keys, [1, 2, 3]);
    // Pending keys are not checked again.
    w.wait_for(&mut batch(&[("d", &[2, 3])]));
    assert_eq!(asked.borrow().len(), 3);
    w.notify(&[2, 3]);
    assert_eq!(ready(&mut w), ["a", "b", "c", "d"]);
}

#[test]
fn slots_are_reused() {
    let (mut w, _) = waiter(&[]);
    for round in 0..10 {
        w.wait_for(&mut batch(&[("a", &[round]), ("b", &[round])]));
        w.notify(&[round]);
        assert_eq!(ready(&mut w), ["a", "b"]);
    }
    assert_eq!(w.waiting(), 0);
}

/// A xorshift generator: deterministic, and enough for shuffling.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Keys become visible to the check, and their notifications follow later,
/// interleaved with waits. No item is ever ready before all its keys are
/// visible, or twice; once every notification is delivered, exactly the
/// items whose keys are all visible are ready.
#[test]
fn matches_the_model() {
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let visible = Rc::new(RefCell::new(HashSet::new()));
        let check = visible.clone();
        let mut waiter: Waiter<u32, u32, _> =
            Waiter::new(move |key: &u32| check.borrow().contains(key));
        // Few keys, many items: keys gather many waiters.
        let keys_in_play = 1 + rng.below(12) as u32;
        let mut items: Vec<Vec<u32>> = Vec::new();
        let mut undelivered: Vec<u32> = Vec::new();
        let mut is_ready: Vec<bool> = Vec::new();
        let mut out = Vec::new();
        for _ in 0..rng.below(400) {
            match rng.below(3) {
                // Items waiting on up to eight keys, repeats possible.
                0 => {
                    let mut batch = WaitBatch::new();
                    for _ in 0..rng.below(10) {
                        let keys: Vec<u32> = (0..rng.below(9))
                            .map(|_| rng.below(u64::from(keys_in_play)) as u32)
                            .collect();
                        batch.push(items.len() as u32, keys.iter().copied());
                        items.push(keys);
                        is_ready.push(false);
                    }
                    waiter.wait_for(&mut batch);
                }
                // A key becomes visible; its notification comes later.
                1 => {
                    let key = rng.below(u64::from(keys_in_play)) as u32;
                    if visible.borrow_mut().insert(key) {
                        undelivered.push(key);
                    }
                }
                // Some notifications, in any order.
                _ => {
                    let n = rng.below(undelivered.len() as u64 + 1) as usize;
                    let mut keys = Vec::new();
                    for _ in 0..n {
                        let i = rng.below(undelivered.len() as u64) as usize;
                        keys.push(undelivered.swap_remove(i));
                    }
                    waiter.notify(&keys);
                }
            }
            out.clear();
            waiter.get_ready(&mut out);
            for item in &out {
                let item = *item as usize;
                assert!(!is_ready[item], "seed {seed}: item {item} ready twice");
                is_ready[item] = true;
                let visible = visible.borrow();
                assert!(
                    items[item].iter().all(|k| visible.contains(k)),
                    "seed {seed}: item {item} ready early"
                );
            }
        }
        waiter.notify(&undelivered);
        out.clear();
        waiter.get_ready(&mut out);
        for item in &out {
            assert!(!is_ready[*item as usize], "seed {seed}: ready twice");
            is_ready[*item as usize] = true;
        }
        let visible = visible.borrow();
        for (item, keys) in items.iter().enumerate() {
            let expected = keys.iter().all(|k| visible.contains(k));
            assert_eq!(is_ready[item], expected, "seed {seed}: item {item}");
        }
        let waiting = items.len() - is_ready.iter().filter(|r| **r).count();
        assert_eq!(waiter.waiting(), waiting, "seed {seed}");
    }
}

/// On a worker thread: commands through a queue, ready batches to a sink.
#[test]
fn the_processor_hands_out_ready_batches() {
    use std::sync::{Arc, Mutex, mpsc};
    use waiter::{Command, WaiterProcessor};

    let visible = Arc::new(Mutex::new(HashSet::from([1u32])));
    let check = visible.clone();
    let (commands, inbox) = workqueue::queue(16);
    let (ready, received) = mpsc::channel();
    let _worker = workqueue::Worker::new("waiter")
        .run(
            inbox,
            move || WaiterProcessor::new(move |key: &u32| check.lock().unwrap().contains(key)),
            move |batch: Vec<u32>| ready.send(batch).unwrap(),
        )
        .spawn();
    let mut batch = WaitBatch::new();
    batch.push(10, [1]);
    batch.push(11, [1, 2]);
    batch.push(12, [2, 3]);
    let push = |command| {
        commands
            .try_push(command)
            .unwrap_or_else(|_| panic!("queue refused"));
    };
    push(Command::Wait(batch));
    let timeout = std::time::Duration::from_secs(10);
    assert_eq!(received.recv_timeout(timeout).unwrap(), [10]);
    // Producers make a key visible before notifying it.
    visible.lock().unwrap().extend([2, 3]);
    push(Command::Notify(vec![2]));
    assert_eq!(received.recv_timeout(timeout).unwrap(), [11]);
    push(Command::Notify(vec![3]));
    assert_eq!(received.recv_timeout(timeout).unwrap(), [12]);
}
