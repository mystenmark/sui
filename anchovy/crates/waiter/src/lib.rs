// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Work that waits on keys: a transaction on its input objects,
//! checkpointing on effects. An item is enqueued with the keys it waits for
//! (`wait_for`), other processors announce keys as they become available
//! (`notify`), and an item whose keys are all available is handed out
//! (`get_ready`). Every method takes a batch.
//!
//! A key notified before anything waits on it is found by the caller's
//! `Availability` check, so the waiter keeps only pending waits. For no wait
//! to be lost, a key must be visible to the check before it is notified:
//! then if the notification is handled first, the check sees the key; if
//! the wait is, the key is pending and the notification wakes it.
//!
//! One thread owns a waiter (see `WaiterProcessor`), so it takes no locks.

use std::hash::Hash;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use workqueue::Processor;

/// Whether keys are available now: the caller's state, such as the store,
/// that a notification follows.
pub trait Availability<K> {
    /// Clears `available`, then pushes whether each of `keys` is available.
    fn check(&mut self, keys: &[K], available: &mut Vec<bool>);
}

/// A check one key at a time.
impl<K, F: FnMut(&K) -> bool> Availability<K> for F {
    fn check(&mut self, keys: &[K], available: &mut Vec<bool>) {
        available.clear();
        available.extend(keys.iter().map(&mut *self));
    }
}

/// Items and the keys each waits for, laid out flat: three vectors however
/// many items, kept when the batch is reused.
pub struct WaitBatch<K, T> {
    items: Vec<T>,
    /// Where each item's keys end in `keys`.
    ends: Vec<u32>,
    keys: Vec<K>,
}

impl<K, T> Default for WaitBatch<K, T> {
    fn default() -> WaitBatch<K, T> {
        WaitBatch::with_capacity(0, 0)
    }
}

impl<K, T> WaitBatch<K, T> {
    pub fn new() -> WaitBatch<K, T> {
        WaitBatch::default()
    }

    pub fn with_capacity(items: usize, keys: usize) -> WaitBatch<K, T> {
        WaitBatch {
            items: Vec::with_capacity(items),
            ends: Vec::with_capacity(items),
            keys: Vec::with_capacity(keys),
        }
    }

    /// Adds `item`, waiting for `keys`.
    pub fn push(&mut self, item: T, keys: impl IntoIterator<Item = K>) {
        self.keys.extend(keys);
        self.ends
            .push(u32::try_from(self.keys.len()).expect("fewer than 2^32 keys in a batch"));
        self.items.push(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.ends.clear();
        self.keys.clear();
    }
}

/// The items waiting on a key, by slot. Most keys have one or two.
enum Waiters {
    One(u32),
    Two(u32, u32),
    /// Three or more, in a vector from the waiter's pool.
    Many(Vec<u32>),
}

impl Waiters {
    /// Adds `slot`, unless it was the last added: an item's keys are
    /// registered together, so a key it repeats has it last. Spilling to
    /// `Many` takes a vector from `spare`.
    fn push(&mut self, slot: u32, spare: &mut Vec<Vec<u32>>) -> bool {
        match self {
            Waiters::One(a) => {
                if *a == slot {
                    return false;
                }
                *self = Waiters::Two(*a, slot);
            }
            Waiters::Two(a, b) => {
                if *b == slot {
                    return false;
                }
                let mut slots = spare.pop().unwrap_or_default();
                slots.extend([*a, *b, slot]);
                *self = Waiters::Many(slots);
            }
            Waiters::Many(slots) => {
                if slots.last() == Some(&slot) {
                    return false;
                }
                slots.push(slot);
            }
        }
        true
    }
}

struct Slot<T> {
    item: Option<T>,
    /// The item's keys not yet available.
    pending: u32,
}

/// Items waiting on keys of type `K`. See the crate documentation.
pub struct Waiter<K, T, A> {
    /// Each pending key's waiting items.
    pending: HashMap<K, Waiters, foldhash::fast::RandomState>,
    /// Waiting items, by slot; free slots are reused first.
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    /// Items ready, in the order they became ready.
    ready: Vec<T>,
    availability: A,
    /// Scratch for `wait_for`'s availability check.
    checking: Vec<K>,
    checked: Vec<bool>,
    /// Vectors for keys with three or more waiters, kept once released.
    spare: Vec<Vec<u32>>,
}

impl<K: Hash + Eq + Clone, T, A: Availability<K>> Waiter<K, T, A> {
    pub fn new(availability: A) -> Waiter<K, T, A> {
        Waiter {
            pending: HashMap::default(),
            slots: Vec::new(),
            free: Vec::new(),
            ready: Vec::new(),
            availability,
            checking: Vec::new(),
            checked: Vec::new(),
            spare: Vec::new(),
        }
    }

    /// Enqueues each item of `batch` until its keys are available, leaving
    /// the batch empty. An item whose keys are all available already is
    /// ready at once. The batch's keys not pending yet are checked in one
    /// call.
    pub fn wait_for(&mut self, batch: &mut WaitBatch<K, T>) {
        // 1. Register each item on each of its keys. A key not pending yet
        //    is registered too, and checked below: one lookup per key.
        let mut keys = batch.keys.drain(..);
        let mut start = 0;
        for (i, item) in batch.items.drain(..).enumerate() {
            let end = batch.ends[i] as usize;
            let slot = self.alloc_slot();
            let mut pending = 0;
            for key in keys.by_ref().take(end - start) {
                match self.pending.entry(key) {
                    Entry::Occupied(mut waiters) => {
                        if waiters.get_mut().push(slot, &mut self.spare) {
                            pending += 1;
                        }
                    }
                    Entry::Vacant(vacant) => {
                        self.checking.push(vacant.key().clone());
                        vacant.insert(Waiters::One(slot));
                        pending += 1;
                    }
                }
            }
            start = end;
            if pending == 0 {
                self.free.push(slot);
                self.ready.push(item);
            } else {
                self.slots[slot as usize] = Slot {
                    item: Some(item),
                    pending,
                };
            }
        }
        drop(keys);
        batch.ends.clear();

        // 2. The keys that were not pending, checked in one call: those
        //    available count off as if notified.
        if !self.checking.is_empty() {
            let mut checking = std::mem::take(&mut self.checking);
            self.availability.check(&checking, &mut self.checked);
            assert_eq!(self.checked.len(), checking.len(), "a verdict per key");
            for (i, key) in checking.iter().enumerate() {
                if self.checked[i] {
                    self.release(key);
                }
            }
            checking.clear();
            self.checking = checking;
        }
    }

    /// Each of `keys` is available from now on: the items waiting on it
    /// count it off, and those with none left become ready. A key nothing
    /// waits on is forgotten.
    pub fn notify(&mut self, keys: &[K]) {
        for key in keys {
            self.release(key);
        }
    }

    /// `key` is available: its waiters count it off.
    fn release(&mut self, key: &K) {
        let Some(waiters) = self.pending.remove(key) else {
            return;
        };
        match waiters {
            Waiters::One(a) => self.count_off(a),
            Waiters::Two(a, b) => {
                self.count_off(a);
                self.count_off(b);
            }
            Waiters::Many(mut slots) => {
                for slot in &slots {
                    self.count_off(*slot);
                }
                slots.clear();
                self.spare.push(slots);
            }
        }
    }

    /// A free slot, reused if one is.
    fn alloc_slot(&mut self) -> u32 {
        if let Some(slot) = self.free.pop() {
            return slot;
        }
        self.slots.push(Slot {
            item: None,
            pending: 0,
        });
        u32::try_from(self.slots.len() - 1).expect("fewer than 2^32 waiting items")
    }

    fn count_off(&mut self, slot: u32) {
        let entry = &mut self.slots[slot as usize];
        entry.pending -= 1;
        if entry.pending == 0 {
            self.ready
                .push(entry.item.take().expect("a waiting slot holds its item"));
            self.free.push(slot);
        }
    }

    /// Moves the ready items to `out`, in the order they became ready.
    pub fn get_ready(&mut self, out: &mut Vec<T>) {
        out.append(&mut self.ready);
    }

    /// Items waiting.
    pub fn waiting(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Keys pending.
    pub fn pending_keys(&self) -> usize {
        self.pending.len()
    }
}

/// What a `WaiterProcessor` takes.
pub enum Command<K, T> {
    Wait(WaitBatch<K, T>),
    Notify(Vec<K>),
}

/// A waiter on a worker thread: after each command, the items it made
/// ready go to the sink as one batch.
pub struct WaiterProcessor<K, T, A> {
    waiter: Waiter<K, T, A>,
}

impl<K: Hash + Eq + Clone, T, A: Availability<K>> WaiterProcessor<K, T, A> {
    pub fn new(availability: A) -> WaiterProcessor<K, T, A> {
        WaiterProcessor {
            waiter: Waiter::new(availability),
        }
    }
}

impl<K, T, A> Processor for WaiterProcessor<K, T, A>
where
    K: Hash + Eq + Clone + Send + 'static,
    T: Send + 'static,
    A: Availability<K>,
{
    type Input = Command<K, T>;
    type Output = Vec<T>;

    fn process(&mut self, command: Command<K, T>) -> Option<Vec<T>> {
        match command {
            Command::Wait(mut batch) => self.waiter.wait_for(&mut batch),
            Command::Notify(keys) => self.waiter.notify(&keys),
        }
        let mut ready = Vec::new();
        self.waiter.get_ready(&mut ready);
        (!ready.is_empty()).then_some(ready)
    }
}
