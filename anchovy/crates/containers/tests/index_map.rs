// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `IndexMap`, `IndexSet` and `BTreeSet` against the `indexmap` crate's and
//! the standard library's, over random operation sequences: every result
//! and the full iteration order agree after each operation.

use containers::{BTreeSet, Bump, Entry, IndexMap, IndexSet};

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn index_map_matches_indexmap() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..200 {
        let bump = Bump::with_capacity(1 << 12);
        let mut ours: IndexMap<u16, u32> = IndexMap::with_capacity_in(4, &bump);
        let mut theirs: indexmap::IndexMap<u16, u32> = indexmap::IndexMap::new();
        for step in 0..300 {
            // Few keys, so operations hit existing entries often.
            let key = rng.below(40) as u16;
            let value = rng.next() as u32;
            match rng.below(12) {
                0..=3 => assert_eq!(ours.insert_full(key, value), theirs.insert_full(key, value)),
                4 => assert_eq!(ours.swap_remove(&key), theirs.swap_remove(&key)),
                5 => assert_eq!(ours.shift_remove(&key), theirs.shift_remove(&key)),
                6 => {
                    let i = rng.below(ours.len() as u64 + 1) as usize;
                    assert_eq!(ours.swap_remove_index(i), theirs.swap_remove_index(i));
                }
                7 => {
                    let i = rng.below(ours.len() as u64 + 1) as usize;
                    assert_eq!(ours.shift_remove_index(i), theirs.shift_remove_index(i));
                }
                8 => assert_eq!(ours.pop(), theirs.pop()),
                9 => {
                    let modulus = rng.below(5) as u32 + 2;
                    ours.retain(|_, v| *v % modulus != 0);
                    theirs.retain(|_, v| *v % modulus != 0);
                }
                10 => {
                    let ours_entry = match ours.entry(key) {
                        Entry::Occupied(mut e) => {
                            *e.get_mut() = value;
                            (e.index(), true)
                        }
                        Entry::Vacant(e) => {
                            let index = e.index();
                            e.insert(value);
                            (index, false)
                        }
                    };
                    let theirs_entry = match theirs.entry(key) {
                        indexmap::map::Entry::Occupied(mut e) => {
                            *e.get_mut() = value;
                            (e.index(), true)
                        }
                        indexmap::map::Entry::Vacant(e) => {
                            let index = e.index();
                            e.insert(value);
                            (index, false)
                        }
                    };
                    assert_eq!(ours_entry, theirs_entry);
                }
                _ => {
                    assert_eq!(ours.get(&key), theirs.get(&key));
                    assert_eq!(ours.get_index_of(&key), theirs.get_index_of(&key));
                    if let Some(v) = ours.get_mut(&key) {
                        *v = value;
                        *theirs.get_mut(&key).unwrap() = value;
                    }
                }
            }
            let ours_order: Vec<(u16, u32)> = ours.iter().map(|(k, v)| (*k, *v)).collect();
            let theirs_order: Vec<(u16, u32)> = theirs.iter().map(|(k, v)| (*k, *v)).collect();
            assert_eq!(ours_order, theirs_order, "after step {step}");
            for (k, v) in &theirs {
                assert_eq!(ours.get(k), Some(v));
            }
            assert_eq!(
                ours.first().map(|(k, _)| *k),
                theirs.first().map(|(k, _)| *k)
            );
            assert_eq!(ours.last().map(|(k, _)| *k), theirs.last().map(|(k, _)| *k));
        }
        let ours_order: Vec<(u16, u32)> = ours.into_iter().collect();
        let theirs_order: Vec<(u16, u32)> = theirs.into_iter().collect();
        assert_eq!(ours_order, theirs_order);
    }
}

#[test]
fn index_set_matches_indexset() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..200 {
        let bump = Bump::with_capacity(1 << 12);
        let mut ours: IndexSet<u16> = IndexSet::new_in(&bump);
        let mut theirs: indexmap::IndexSet<u16> = indexmap::IndexSet::new();
        for _ in 0..300 {
            let value = rng.below(40) as u16;
            match rng.below(6) {
                0..=2 => assert_eq!(ours.insert_full(value), theirs.insert_full(value)),
                3 => assert_eq!(ours.swap_remove(&value), theirs.swap_remove(&value)),
                4 => assert_eq!(ours.shift_remove(&value), theirs.shift_remove(&value)),
                _ => assert_eq!(ours.contains(&value), theirs.contains(&value)),
            }
            assert!(ours.iter().eq(theirs.iter()));
        }
    }
}

#[test]
fn btree_set_matches_std() {
    let mut rng = Rng(0x0f0f_f0f0_1234_4321);
    let bump = Bump::with_capacity(1 << 12);
    let mut ours: BTreeSet<u16> = BTreeSet::new_in(&bump);
    let mut theirs = std::collections::BTreeSet::new();
    for _ in 0..2000 {
        let value = rng.below(64) as u16;
        match rng.below(4) {
            0 | 1 => assert_eq!(ours.insert(value), theirs.insert(value)),
            2 => assert_eq!(ours.remove(&value), theirs.remove(&value)),
            _ => assert_eq!(ours.pop_first(), theirs.pop_first()),
        }
        assert!(ours.iter().eq(theirs.iter()));
        assert_eq!(ours.first(), theirs.first());
        assert_eq!(ours.last(), theirs.last());
    }
}
