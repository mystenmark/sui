// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! An insertion-ordered map and set in a [`Bump`], after the `indexmap`
//! crate's, which has no allocator support. Entries live in a vector in
//! order; a hash table maps each key to its entry's index. Method names
//! and semantics are `indexmap`'s, so ported code reads the same; as
//! there, removal either swaps the last entry into the gap (`swap_*`,
//! O(1)) or shifts the rest down (`shift_*`, O(n)), and there is no
//! ambiguous `remove`.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};

use foldhash::fast::RandomState;
use hashbrown::HashTable;

use crate::{Bump, Vec};

struct Bucket<K, V> {
    hash: u64,
    key: K,
    value: V,
}

pub struct IndexMap<'a, K, V> {
    entries: Vec<'a, Bucket<K, V>>,
    /// Indices into `entries`, hashed by their entry's key.
    indices: HashTable<usize, &'a Bump>,
    hasher: RandomState,
}

impl<'a, K, V> IndexMap<'a, K, V> {
    pub fn new_in(bump: &'a Bump) -> IndexMap<'a, K, V> {
        IndexMap::with_capacity_in(0, bump)
    }

    pub fn with_capacity_in(capacity: usize, bump: &'a Bump) -> IndexMap<'a, K, V> {
        IndexMap {
            entries: Vec::with_capacity_in(capacity, bump),
            indices: HashTable::with_capacity_in(capacity, bump),
            hasher: RandomState::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.indices.clear();
    }

    pub fn get_index(&self, index: usize) -> Option<(&K, &V)> {
        self.entries.get(index).map(|b| (&b.key, &b.value))
    }

    pub fn get_index_mut(&mut self, index: usize) -> Option<(&K, &mut V)> {
        self.entries.get_mut(index).map(|b| (&b.key, &mut b.value))
    }

    pub fn first(&self) -> Option<(&K, &V)> {
        self.get_index(0)
    }

    pub fn last(&self) -> Option<(&K, &V)> {
        self.entries.last().map(|b| (&b.key, &b.value))
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.entries.iter())
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.entries.iter_mut())
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> + ExactSizeIterator {
        self.entries.iter().map(|b| &b.key)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + ExactSizeIterator {
        self.entries.iter().map(|b| &b.value)
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut V> + ExactSizeIterator {
        self.entries.iter_mut().map(|b| &mut b.value)
    }

    /// Removes the entry at `index`, moving the last entry into its place.
    pub fn swap_remove_index(&mut self, index: usize) -> Option<(K, V)> {
        let hash = self.entries.get(index)?.hash;
        self.erase_index(hash, index);
        let removed = self.entries.swap_remove(index);
        if let Some(moved) = self.entries.get(index) {
            let last = self.entries.len();
            *self
                .indices
                .find_mut(moved.hash, |&i| i == last)
                .expect("every entry is indexed") = index;
        }
        Some((removed.key, removed.value))
    }

    /// Removes the entry at `index`, shifting those after it down.
    pub fn shift_remove_index(&mut self, index: usize) -> Option<(K, V)> {
        let hash = self.entries.get(index)?.hash;
        self.erase_index(hash, index);
        let removed = self.entries.remove(index);
        for i in &mut self.indices {
            if *i > index {
                *i -= 1;
            }
        }
        Some((removed.key, removed.value))
    }

    pub fn pop(&mut self) -> Option<(K, V)> {
        let last = self.entries.len().checked_sub(1)?;
        self.swap_remove_index(last)
    }

    /// Keeps the entries `keep` accepts, in order.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let before = self.entries.len();
        self.entries.retain_mut(|b| keep(&b.key, &mut b.value));
        if self.entries.len() != before {
            self.rebuild_indices();
        }
    }

    fn erase_index(&mut self, hash: u64, index: usize) {
        self.indices
            .find_entry(hash, |&i| i == index)
            .expect("every entry is indexed")
            .remove();
    }

    fn rebuild_indices(&mut self) {
        self.indices.clear();
        let entries = &self.entries;
        for (i, b) in entries.iter().enumerate() {
            self.indices.insert_unique(b.hash, i, |&j| entries[j].hash);
        }
    }
}

impl<'a, K: Hash + Eq, V> IndexMap<'a, K, V> {
    fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
        self.hasher.hash_one(key)
    }

    pub fn get_index_of<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        let entries = &self.entries;
        self.indices
            .find(hash, |&i| entries[i].key.borrow() == key)
            .copied()
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_index_of(key).is_some()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_index_of(key).map(|i| &self.entries[i].value)
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.get_index_of(key)?;
        Some(&mut self.entries[i].value)
    }

    pub fn get_full<Q>(&self, key: &Q) -> Option<(usize, &K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.get_index_of(key)?;
        let b = &self.entries[i];
        Some((i, &b.key, &b.value))
    }

    /// Inserts, keeping an existing key's position (and the existing key);
    /// the replaced value, if any.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.insert_full(key, value).1
    }

    /// As [`IndexMap::insert`], with the entry's index.
    pub fn insert_full(&mut self, key: K, value: V) -> (usize, Option<V>) {
        match self.entry(key) {
            Entry::Occupied(mut e) => (e.index(), Some(e.insert(value))),
            Entry::Vacant(e) => {
                let index = e.index();
                e.insert(value);
                (index, None)
            }
        }
    }

    pub fn entry(&mut self, key: K) -> Entry<'_, 'a, K, V> {
        let hash = self.hash(&key);
        let entries = &self.entries;
        match self.indices.find(hash, |&i| entries[i].key == key) {
            Some(&index) => Entry::Occupied(OccupiedEntry { map: self, index }),
            None => Entry::Vacant(VacantEntry {
                map: self,
                hash,
                key,
            }),
        }
    }

    pub fn swap_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.get_index_of(key)?;
        self.swap_remove_index(i).map(|(_, v)| v)
    }

    pub fn shift_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.get_index_of(key)?;
        self.shift_remove_index(i).map(|(_, v)| v)
    }

    fn push(&mut self, hash: u64, key: K, value: V) -> usize {
        let index = self.entries.len();
        self.entries.push(Bucket { hash, key, value });
        let entries = &self.entries;
        self.indices
            .insert_unique(hash, index, |&j| entries[j].hash);
        index
    }
}

impl<K: Hash + Eq, V> Extend<(K, V)> for IndexMap<'_, K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IndexMap<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

pub enum Entry<'m, 'a, K, V> {
    Occupied(OccupiedEntry<'m, 'a, K, V>),
    Vacant(VacantEntry<'m, 'a, K, V>),
}

impl<'m, K: Hash + Eq, V> Entry<'m, '_, K, V> {
    pub fn index(&self) -> usize {
        match self {
            Entry::Occupied(e) => e.index(),
            Entry::Vacant(e) => e.index(),
        }
    }

    pub fn key(&self) -> &K {
        match self {
            Entry::Occupied(e) => e.key(),
            Entry::Vacant(e) => e.key(),
        }
    }

    pub fn or_insert(self, default: V) -> &'m mut V {
        match self {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(default),
        }
    }

    pub fn or_insert_with(self, default: impl FnOnce() -> V) -> &'m mut V {
        match self {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(default()),
        }
    }

    pub fn or_default(self) -> &'m mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }

    #[must_use]
    pub fn and_modify(mut self, f: impl FnOnce(&mut V)) -> Self {
        if let Entry::Occupied(e) = &mut self {
            f(e.get_mut());
        }
        self
    }
}

pub struct OccupiedEntry<'m, 'a, K, V> {
    map: &'m mut IndexMap<'a, K, V>,
    index: usize,
}

impl<'m, K, V> OccupiedEntry<'m, '_, K, V> {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn key(&self) -> &K {
        &self.map.entries[self.index].key
    }

    pub fn get(&self) -> &V {
        &self.map.entries[self.index].value
    }

    pub fn get_mut(&mut self) -> &mut V {
        &mut self.map.entries[self.index].value
    }

    pub fn into_mut(self) -> &'m mut V {
        &mut self.map.entries[self.index].value
    }

    /// Replaces the value; the old one.
    pub fn insert(&mut self, value: V) -> V {
        core::mem::replace(self.get_mut(), value)
    }

    pub fn swap_remove(self) -> V {
        self.map
            .swap_remove_index(self.index)
            .expect("an occupied entry's index is in bounds")
            .1
    }

    pub fn shift_remove(self) -> V {
        self.map
            .shift_remove_index(self.index)
            .expect("an occupied entry's index is in bounds")
            .1
    }
}

pub struct VacantEntry<'m, 'a, K, V> {
    map: &'m mut IndexMap<'a, K, V>,
    hash: u64,
    key: K,
}

impl<'m, K: Hash + Eq, V> VacantEntry<'m, '_, K, V> {
    /// The index the entry will have.
    pub fn index(&self) -> usize {
        self.map.len()
    }

    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn into_key(self) -> K {
        self.key
    }

    pub fn insert(self, value: V) -> &'m mut V {
        let index = self.map.push(self.hash, self.key, value);
        &mut self.map.entries[index].value
    }
}

pub struct Iter<'m, K, V>(core::slice::Iter<'m, Bucket<K, V>>);

impl<'m, K, V> Iterator for Iter<'m, K, V> {
    type Item = (&'m K, &'m V);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|b| (&b.key, &b.value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for Iter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|b| (&b.key, &b.value))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}

pub struct IterMut<'m, K, V>(core::slice::IterMut<'m, Bucket<K, V>>);

impl<'m, K, V> Iterator for IterMut<'m, K, V> {
    type Item = (&'m K, &'m mut V);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|b| (&b.key, &mut b.value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for IterMut<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|b| (&b.key, &mut b.value))
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {}

pub struct IntoIter<'a, K, V>(allocator_api2::vec::IntoIter<Bucket<K, V>, &'a Bump>);

impl<K, V> Iterator for IntoIter<'_, K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|b| (b.key, b.value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, V> DoubleEndedIterator for IntoIter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|b| (b.key, b.value))
    }
}

impl<K, V> ExactSizeIterator for IntoIter<'_, K, V> {}

impl<'a, K, V> IntoIterator for IndexMap<'a, K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<'a, K, V>;
    fn into_iter(self) -> IntoIter<'a, K, V> {
        IntoIter(self.entries.into_iter())
    }
}

impl<'m, K, V> IntoIterator for &'m IndexMap<'_, K, V> {
    type Item = (&'m K, &'m V);
    type IntoIter = Iter<'m, K, V>;
    fn into_iter(self) -> Iter<'m, K, V> {
        self.iter()
    }
}

impl<'m, K, V> IntoIterator for &'m mut IndexMap<'_, K, V> {
    type Item = (&'m K, &'m mut V);
    type IntoIter = IterMut<'m, K, V>;
    fn into_iter(self) -> IterMut<'m, K, V> {
        self.iter_mut()
    }
}

/// `indexmap`'s `IndexSet`: an [`IndexMap`] to `()`.
pub struct IndexSet<'a, T> {
    map: IndexMap<'a, T, ()>,
}

impl<'a, T> IndexSet<'a, T> {
    pub fn new_in(bump: &'a Bump) -> IndexSet<'a, T> {
        IndexSet {
            map: IndexMap::new_in(bump),
        }
    }

    pub fn with_capacity_in(capacity: usize, bump: &'a Bump) -> IndexSet<'a, T> {
        IndexSet {
            map: IndexMap::with_capacity_in(capacity, bump),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn get_index(&self, index: usize) -> Option<&T> {
        self.map.get_index(index).map(|(k, ())| k)
    }

    pub fn first(&self) -> Option<&T> {
        self.map.first().map(|(k, ())| k)
    }

    pub fn last(&self) -> Option<&T> {
        self.map.last().map(|(k, ())| k)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.map.keys()
    }

    pub fn swap_remove_index(&mut self, index: usize) -> Option<T> {
        self.map.swap_remove_index(index).map(|(k, ())| k)
    }

    pub fn shift_remove_index(&mut self, index: usize) -> Option<T> {
        self.map.shift_remove_index(index).map(|(k, ())| k)
    }

    pub fn pop(&mut self) -> Option<T> {
        self.map.pop().map(|(k, ())| k)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.map.retain(|k, ()| keep(k));
    }
}

impl<T: Hash + Eq> IndexSet<'_, T> {
    /// Inserts `value` unless present (keeping the present one and its
    /// position); whether it was inserted.
    pub fn insert(&mut self, value: T) -> bool {
        self.insert_full(value).1
    }

    /// As [`IndexSet::insert`], with the value's index.
    pub fn insert_full(&mut self, value: T) -> (usize, bool) {
        match self.map.entry(value) {
            Entry::Occupied(e) => (e.index(), false),
            Entry::Vacant(e) => {
                let index = e.index();
                e.insert(());
                (index, true)
            }
        }
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(value)
    }

    pub fn get_index_of<Q>(&self, value: &Q) -> Option<usize>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_index_of(value)
    }

    pub fn get<Q>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_full(value).map(|(_, k, ())| k)
    }

    pub fn swap_remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.swap_remove(value).is_some()
    }

    pub fn shift_remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.shift_remove(value).is_some()
    }
}

impl<T: Hash + Eq> Extend<T> for IndexSet<'_, T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for v in iter {
            self.insert(v);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for IndexSet<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<'a, T> IntoIterator for IndexSet<'a, T> {
    type Item = T;
    type IntoIter = core::iter::Map<IntoIter<'a, T, ()>, fn((T, ())) -> T>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.into_iter().map(|(k, ())| k)
    }
}
