// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A map built once and then only read: entries sorted by key in one arena vector, looked up by
//! binary search. Reads and iteration answer as a [`crate::BTreeMap`] built by inserting the
//! same entries in the same order would.

use core::borrow::Borrow;
use core::fmt;

use crate::{Bump, Vec};

pub struct VecMap<'a, K, V> {
    /// Sorted by key, keys unique.
    entries: Vec<'a, (K, V)>,
}

impl<'a, K: Ord, V> VecMap<'a, K, V> {
    pub fn new_in(bump: &'a Bump) -> VecMap<'a, K, V> {
        VecMap {
            entries: Vec::new_in(bump),
        }
    }

    /// The map of `entries` inserted in order: for a repeated key, the last value wins, as
    /// `BTreeMap::insert` would leave it.
    pub fn from_entries(mut entries: Vec<'a, (K, V)>) -> VecMap<'a, K, V> {
        // Stable, so equal keys keep their insertion order and the last can be kept.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut write = 0;
        for read in 0..entries.len() {
            if write > 0 && entries[write - 1].0 == entries[read].0 {
                entries.swap(write - 1, read);
            } else {
                entries.swap(write, read);
                write += 1;
            }
        }
        entries.truncate(write);
        VecMap { entries }
    }

    fn position<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.entries
            .binary_search_by(|(k, _)| k.borrow().cmp(key))
            .ok()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key).map(|i| &self.entries[i].1)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key).is_some()
    }
}

impl<K, V> VecMap<'_, K, V> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// In key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> + ExactSizeIterator {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> + ExactSizeIterator {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + ExactSizeIterator {
        self.entries.iter().map(|(_, v)| v)
    }

    /// The entries, sorted by key.
    pub fn as_slice(&self) -> &[(K, V)] {
        &self.entries
    }
}

impl<K: Ord, V, Q: Ord + ?Sized> core::ops::Index<&Q> for VecMap<'_, K, V>
where
    K: Borrow<Q>,
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("key in the map")
    }
}

impl<'m, K, V> IntoIterator for &'m VecMap<'_, K, V> {
    type Item = (&'m K, &'m V);
    type IntoIter = core::iter::Map<core::slice::Iter<'m, (K, V)>, fn(&(K, V)) -> (&K, &V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}

impl<'a, K, V> IntoIterator for VecMap<'a, K, V> {
    type Item = (K, V);
    type IntoIter = allocator_api2::vec::IntoIter<(K, V), &'a Bump>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for VecMap<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}
