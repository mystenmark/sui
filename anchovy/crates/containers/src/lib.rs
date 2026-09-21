// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The containers used throughout the code, all allocating from a
//! [`Bump`] so that a whole computation's temporaries are one free.

use std::hash::{BuildHasher, Hasher};

pub use arena::Bump;

pub type Vec<'a, T> = allocator_api2::vec::Vec<T, &'a Bump>;
pub type Box<'a, T> = allocator_api2::boxed::Box<T, &'a Bump>;

/// The standard library's table (`hashbrown`) with its default hasher.
pub type HashMap<'a, K, V> = hashbrown::HashMap<K, V, foldhash::fast::RandomState, &'a Bump>;
pub type HashSet<'a, K> = hashbrown::HashSet<K, foldhash::fast::RandomState, &'a Bump>;

/// The standard library's `BTreeMap`, ported to stable with allocator
/// support by `arena-btreemap`.
pub type BTreeMap<'a, K, V> = arena_btreemap::BTreeMap<K, V, &'a Bump>;

pub fn hash_map<K, V>(bump: &Bump, capacity: usize) -> HashMap<'_, K, V> {
    HashMap::with_capacity_and_hasher_in(capacity, foldhash::fast::RandomState::default(), bump)
}

pub fn hash_set<K>(bump: &Bump, capacity: usize) -> HashSet<'_, K> {
    HashSet::with_capacity_and_hasher_in(capacity, foldhash::fast::RandomState::default(), bump)
}

/// A map keyed by message digest. Digests are uniformly distributed
/// already, so the hash is their first eight bytes and nothing is hashed.
///
/// The key's `Hash` impl must call `write_u64` once with those bytes and
/// nothing else; `messages::base::Digest` does.
pub type MessageMap<'a, K, V> = hashbrown::HashMap<K, V, DigestHasher, &'a Bump>;
pub type MessageSet<'a, K> = hashbrown::HashSet<K, DigestHasher, &'a Bump>;

pub fn message_map<K, V>(bump: &Bump, capacity: usize) -> MessageMap<'_, K, V> {
    MessageMap::with_capacity_and_hasher_in(capacity, DigestHasher, bump)
}

pub fn message_set<K>(bump: &Bump, capacity: usize) -> MessageSet<'_, K> {
    MessageSet::with_capacity_and_hasher_in(capacity, DigestHasher, bump)
}

#[derive(Clone, Copy, Default, Debug)]
pub struct DigestHasher;

impl BuildHasher for DigestHasher {
    type Hasher = DigestHasher64;

    fn build_hasher(&self) -> DigestHasher64 {
        DigestHasher64(0)
    }
}

#[derive(Default)]
pub struct DigestHasher64(u64);

impl Hasher for DigestHasher64 {
    #[inline]
    fn write_u64(&mut self, v: u64) {
        debug_assert_eq!(self.0, 0, "a digest key hashes exactly once");
        self.0 = v;
    }

    fn write(&mut self, _: &[u8]) {
        unreachable!("digest keys hash by write_u64 only")
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// A map built once and read many times: entries sorted by key in a `Vec`,
/// looked up by binary search, or a scan when there are few.
pub struct SortedMap<'a, K, V> {
    entries: Vec<'a, (K, V)>,
}

/// Below this many entries a scan beats a binary search.
const SCAN_BELOW: usize = 16;

impl<'a, K: Ord, V> SortedMap<'a, K, V> {
    pub fn new_in(bump: &'a Bump, capacity: usize) -> Self {
        SortedMap {
            entries: Vec::with_capacity_in(capacity, bump),
        }
    }

    /// Sorts the entries by key; of equal keys the last one given wins.
    pub fn from_iter_in(bump: &'a Bump, iter: impl IntoIterator<Item = (K, V)>) -> Self {
        let iter = iter.into_iter();
        let mut entries = Vec::with_capacity_in(iter.size_hint().0, bump);
        entries.extend(iter);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        // Of a run of equal keys keep the last, which `sort_by` (stable) put
        // at the end of the run.
        let mut kept = 0;
        for i in 0..entries.len() {
            if i + 1 < entries.len() && entries[i + 1].0 == entries[i].0 {
                continue;
            }
            entries.swap(kept, i);
            kept += 1;
        }
        entries.truncate(kept);
        SortedMap { entries }
    }

    /// Appends an entry whose key is greater than every key so far.
    pub fn push(&mut self, key: K, value: V) {
        debug_assert!(self.entries.last().is_none_or(|(k, _)| *k < key));
        self.entries.push((key, value));
    }

    fn position(&self, key: &K) -> Result<usize, usize> {
        if self.entries.len() < SCAN_BELOW {
            for (i, (k, _)) in self.entries.iter().enumerate() {
                match k.cmp(key) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => return Ok(i),
                    std::cmp::Ordering::Greater => return Err(i),
                }
            }
            Err(self.entries.len())
        } else {
            self.entries.binary_search_by(|(k, _)| k.cmp(key))
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.position(key).ok().map(|i| &self.entries[i].1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        match self.position(key) {
            Ok(i) => Some(&mut self.entries[i].1),
            Err(_) => None,
        }
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.position(key).is_ok()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn as_slice(&self) -> &[(K, V)] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    #[test]
    fn sorted_map_lookups() {
        let bump = Bump::with_capacity(4096);
        let small = SortedMap::from_iter_in(&bump, [(3, "c"), (1, "a"), (2, "b"), (1, "A")]);
        assert_eq!(small.as_slice(), &[(1, "A"), (2, "b"), (3, "c")]);
        assert_eq!(small.get(&2), Some(&"b"));
        assert_eq!(small.get(&4), None);

        let large = SortedMap::from_iter_in(&bump, (0..100).rev().map(|i| (i, i * i)));
        assert_eq!(large.len(), 100);
        assert_eq!(large.get(&77), Some(&5929));
        assert!(!large.contains_key(&100));

        let mut pushed = SortedMap::new_in(&bump, 3);
        pushed.push(1, ());
        pushed.push(5, ());
        assert!(pushed.contains_key(&5));
        assert_eq!(bump.chunks(), 1);
    }

    #[derive(PartialEq, Eq, Clone, Copy)]
    struct Key([u8; 32]);

    impl Hash for Key {
        fn hash<H: Hasher>(&self, state: &mut H) {
            state.write_u64(u64::from_le_bytes(self.0[..8].try_into().unwrap()));
        }
    }

    #[test]
    fn every_container_lives_in_the_arena() {
        let bump = Bump::with_capacity(1 << 20);
        let mut v: Vec<u64> = Vec::new_in(&bump);
        v.extend(0..1000);
        let mut h = hash_map(&bump, 16);
        for i in 0..1000u64 {
            h.insert(i, i);
        }
        let mut m = message_map(&bump, 16);
        for i in 0..=255u8 {
            m.insert(Key([i; 32]), i);
        }
        assert_eq!(m.get(&Key([7; 32])), Some(&7));
        let mut b: BTreeMap<u64, u64> = BTreeMap::new_in(&bump);
        for i in (0..1000).rev() {
            b.insert(i, i);
        }
        assert_eq!(b.iter().next(), Some((&0, &0)));
        let boxed = Box::new_in(42u64, &bump);
        assert_eq!(*boxed, 42);
        assert_eq!(bump.chunks(), 1);
        assert!(bump.allocated() > 0);
    }
}
