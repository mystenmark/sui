// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The standard library's `BTreeSet`, over `arena-btreemap`'s map (which
//! has no set): a [`BTreeMap`] to `()`.

use core::borrow::Borrow;
use core::fmt;

use crate::{BTreeMap, Bump};

pub struct BTreeSet<'a, T> {
    map: BTreeMap<'a, T, ()>,
}

impl<'a, T: Ord> BTreeSet<'a, T> {
    pub fn new_in(bump: &'a Bump) -> BTreeSet<'a, T> {
        BTreeSet {
            map: BTreeMap::new_in(bump),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether `value` was not present (a present one is kept).
    pub fn insert(&mut self, value: T) -> bool {
        if self.map.contains_key(&value) {
            return false;
        }
        self.map.insert(value, ());
        true
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.map.contains_key(value)
    }

    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.map.remove(value).is_some()
    }

    pub fn first(&self) -> Option<&T> {
        self.map.first_key_value().map(|(k, ())| k)
    }

    pub fn last(&self) -> Option<&T> {
        self.map.last_key_value().map(|(k, ())| k)
    }

    pub fn pop_first(&mut self) -> Option<T> {
        self.map.pop_first().map(|(k, ())| k)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.map.keys()
    }

    /// Whether no value is in both.
    pub fn is_disjoint(&self, other: &BTreeSet<'_, T>) -> bool {
        let (small, large) = if self.len() <= other.len() {
            (self, other)
        } else {
            (other, self)
        };
        small.iter().all(|v| !large.contains(v))
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.map.retain(|k, ()| keep(k));
    }
}

impl<T: Ord> Extend<T> for BTreeSet<'_, T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for v in iter {
            self.insert(v);
        }
    }
}

impl<T: fmt::Debug + Ord> fmt::Debug for BTreeSet<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<'m, T> IntoIterator for &'m BTreeSet<'_, T> {
    type Item = &'m T;
    type IntoIter = arena_btreemap::btree::map::Keys<'m, T, ()>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.keys()
    }
}

impl<'a, T> IntoIterator for BTreeSet<'a, T> {
    type Item = T;
    type IntoIter = arena_btreemap::btree::map::IntoKeys<T, (), &'a Bump>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.into_keys()
    }
}

impl<T: Ord> PartialEq for BTreeSet<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<T: Ord> Eq for BTreeSet<'_, T> {}
