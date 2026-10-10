// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `VecMap` against std's `BTreeMap` built by inserting the same entries in order.

use std::collections::BTreeMap;

use containers::{Bump, Vec, VecMap};

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[test]
fn reads_as_a_btree_map() {
    let mut rng = Rng(3);
    for _ in 0..2000 {
        let bump = Bump::with_capacity(1 << 12);
        let n = (rng.next() % 24) as usize;
        let keys = 1 + rng.next() % 16;
        let mut entries = Vec::new_in(&bump);
        let mut reference = BTreeMap::new();
        for i in 0..n {
            let k = rng.next() % keys;
            entries.push((k, i));
            reference.insert(k, i);
        }
        let map = VecMap::from_entries(entries);
        assert_eq!(map.len(), reference.len());
        assert!(map.iter().eq(reference.iter()));
        for k in 0..keys + 2 {
            assert_eq!(map.get(&k), reference.get(&k));
            assert_eq!(map.contains_key(&k), reference.contains_key(&k));
        }
    }
}
