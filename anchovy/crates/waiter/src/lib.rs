// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Work that waits on keys: a transaction on its input objects,
//! checkpointing on effects. An item is enqueued with the keys it waits for
//! (`wait_for`), other processors announce keys as they resolve, each to a
//! value (`resolve`; `notify` where the value is `()`), and an item whose
//! keys have all resolved is handed out with their values (`get_ready`; see
//! `Receive`). Every method takes a batch.
//!
//! A key resolved before anything waits on it is found by the caller's
//! `Availability` check, so the waiter keeps only pending waits. For no wait
//! to be lost, a key must be visible to the check before it is resolved:
//! then if the resolution is handled first, the check finds the key; if the
//! wait is, the key is pending and the resolution wakes it. A key the check
//! cannot find must instead be resolved only after its wait is enqueued,
//! for instance through the same queue.
//!
//! One thread owns a waiter (see `WaiterProcessor`), so it takes no locks.

use std::hash::Hash;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use workqueue::Processor;

/// Which keys have resolved already, and to what: the caller's state, such
/// as the store, that a resolution follows.
pub trait Availability<K, V = ()> {
    /// Clears `found`, then pushes each of `keys`' value, if it has one now.
    fn check(&mut self, keys: &[K], found: &mut Vec<Option<V>>);
}

/// A check one key at a time, of keys without values.
impl<K, F: FnMut(&K) -> bool> Availability<K> for F {
    fn check(&mut self, keys: &[K], found: &mut Vec<Option<()>>) {
        found.clear();
        found.extend(keys.iter().map(|k| self(k).then_some(())));
    }
}

/// What a waiting item does with its keys' values: receives each as its key
/// resolves, by the key's position in what it waited for. Items of waiters
/// without values receive nothing.
pub trait Receive<V> {
    fn receive(&mut self, index: usize, value: V);
}

impl<T> Receive<()> for T {
    fn receive(&mut self, _: usize, (): ()) {}
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

    /// Each item with the keys it waits for.
    pub fn into_parts(self) -> Vec<(T, Vec<K>)> {
        let mut keys = self.keys.into_iter();
        let mut start = 0;
        self.items
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let end = self.ends[i] as usize;
                let item_keys = keys.by_ref().take(end - start).collect();
                start = end;
                (item, item_keys)
            })
            .collect()
    }
}

/// Where a key is waited on: the item's slot, and the key's position among
/// the item's keys.
#[derive(Clone, Copy)]
struct Place {
    slot: u32,
    index: u32,
}

/// The places waiting on a key. Most keys have one or two.
enum Waiters {
    One(Place),
    Two(Place, Place),
    /// Three or more, in a vector from the waiter's pool.
    Many(Vec<Place>),
}

impl Waiters {
    /// Adds `place`; spilling to `Many` takes a vector from `spare`.
    fn push(&mut self, place: Place, spare: &mut Vec<Vec<Place>>) {
        match self {
            Waiters::One(a) => *self = Waiters::Two(*a, place),
            Waiters::Two(a, b) => {
                let mut places = spare.pop().unwrap_or_default();
                places.extend([*a, *b, place]);
                *self = Waiters::Many(places);
            }
            Waiters::Many(places) => places.push(place),
        }
    }
}

struct Slot<T> {
    item: Option<T>,
    /// The item's keys not yet resolved, counting a repeated key each time.
    pending: u32,
}

/// Items waiting on keys of type `K`, each resolving to a `V`. See the
/// crate documentation.
pub struct Waiter<K, T, A, V = ()> {
    /// Each pending key's waiting places.
    pending: HashMap<K, Waiters, foldhash::fast::RandomState>,
    /// Waiting items, by slot; free slots are reused first.
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    /// Items ready, in the order they became ready.
    ready: Vec<T>,
    availability: A,
    /// Scratch for `wait_for`'s availability check.
    checking: Vec<K>,
    checked: Vec<Option<V>>,
    /// Vectors for keys with three or more waiters, kept once released.
    spare: Vec<Vec<Place>>,
}

impl<K, T, A, V> Waiter<K, T, A, V>
where
    K: Hash + Eq + Clone,
    T: Receive<V>,
    A: Availability<K, V>,
    V: Clone,
{
    pub fn new(availability: A) -> Waiter<K, T, A, V> {
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
            let count = end - start;
            let slot = self.alloc_slot();
            for (index, key) in keys.by_ref().take(count).enumerate() {
                let place = Place {
                    slot,
                    index: index as u32,
                };
                match self.pending.entry(key) {
                    Entry::Occupied(mut waiters) => waiters.get_mut().push(place, &mut self.spare),
                    Entry::Vacant(vacant) => {
                        self.checking.push(vacant.key().clone());
                        vacant.insert(Waiters::One(place));
                    }
                }
            }
            start = end;
            if count == 0 {
                self.free.push(slot);
                self.ready.push(item);
            } else {
                self.slots[slot as usize] = Slot {
                    item: Some(item),
                    pending: count as u32,
                };
            }
        }
        drop(keys);
        batch.ends.clear();

        // 2. The keys that were not pending, checked in one call: those
        //    found resolve as if resolved now.
        if !self.checking.is_empty() {
            let mut checking = std::mem::take(&mut self.checking);
            let mut checked = std::mem::take(&mut self.checked);
            self.availability.check(&checking, &mut checked);
            assert_eq!(checked.len(), checking.len(), "a verdict per key");
            for (i, value) in checked.drain(..).enumerate() {
                if let Some(value) = value {
                    self.release(&checking[i], value);
                }
            }
            checking.clear();
            self.checking = checking;
            self.checked = checked;
        }
    }

    /// Each key has resolved, to its value, from now on: the items waiting
    /// on it take the value, and those with no key left become ready. A key
    /// nothing waits on is forgotten.
    pub fn resolve(&mut self, resolved: impl IntoIterator<Item = (K, V)>) {
        for (key, value) in resolved {
            self.release(&key, value);
        }
    }

    /// `key` resolved to `value`: its waiters take it.
    fn release(&mut self, key: &K, value: V) {
        let Some(waiters) = self.pending.remove(key) else {
            return;
        };
        match waiters {
            Waiters::One(a) => self.count_off(a, value),
            Waiters::Two(a, b) => {
                self.count_off(a, value.clone());
                self.count_off(b, value);
            }
            Waiters::Many(mut places) => {
                for place in &places {
                    self.count_off(*place, value.clone());
                }
                places.clear();
                self.spare.push(places);
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

    fn count_off(&mut self, place: Place, value: V) {
        let entry = &mut self.slots[place.slot as usize];
        entry
            .item
            .as_mut()
            .expect("a waiting slot holds its item")
            .receive(place.index as usize, value);
        entry.pending -= 1;
        if entry.pending == 0 {
            self.ready
                .push(entry.item.take().expect("a waiting slot holds its item"));
            self.free.push(place.slot);
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

impl<K, T, A> Waiter<K, T, A>
where
    K: Hash + Eq + Clone,
    A: Availability<K>,
{
    /// Each of `keys` is available from now on: `resolve`, for keys
    /// without values.
    pub fn notify(&mut self, keys: &[K]) {
        for key in keys {
            self.release(key, ());
        }
    }
}

/// What a `WaiterProcessor` takes.
pub enum Command<K, T, V = ()> {
    Wait(WaitBatch<K, T>),
    /// Keys resolved, each to its value.
    Resolve(Vec<(K, V)>),
}

impl<K, T> Command<K, T> {
    /// Keys available, for waiters without values.
    pub fn notify(keys: impl IntoIterator<Item = K>) -> Command<K, T> {
        Command::Resolve(keys.into_iter().map(|k| (k, ())).collect())
    }
}

/// A waiter on a worker thread: after each command, the items it made
/// ready go to the sink as one batch.
pub struct WaiterProcessor<K, T, A, V = ()> {
    waiter: Waiter<K, T, A, V>,
}

impl<K, T, A, V> WaiterProcessor<K, T, A, V>
where
    K: Hash + Eq + Clone,
    T: Receive<V>,
    A: Availability<K, V>,
    V: Clone,
{
    pub fn new(availability: A) -> WaiterProcessor<K, T, A, V> {
        WaiterProcessor {
            waiter: Waiter::new(availability),
        }
    }
}

impl<K, T, A, V> Processor for WaiterProcessor<K, T, A, V>
where
    K: Hash + Eq + Clone + Send + 'static,
    T: Receive<V> + Send + 'static,
    A: Availability<K, V>,
    V: Clone + Send + 'static,
{
    type Input = Command<K, T, V>;
    type Output = Vec<T>;

    fn process(
        &mut self,
        command: Command<K, T, V>,
        _bump: &mut workqueue::Bump,
    ) -> Option<Vec<T>> {
        match command {
            Command::Wait(mut batch) => self.waiter.wait_for(&mut batch),
            Command::Resolve(resolved) => self.waiter.resolve(resolved),
        }
        let mut ready = Vec::new();
        self.waiter.get_ready(&mut ready);
        (!ready.is_empty()).then_some(ready)
    }
}
