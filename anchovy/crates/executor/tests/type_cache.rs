// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The epoch's type cache: buckets by linkage and mode, and its bound on entries.

use containers::Bump;
use executor::static_programmable_transactions::type_cache::{MAX_ENTRIES, TypeCache};
use move_core_types::{account_address::AccountAddress, language_storage::TypeTag};
use move_vm_runtime::shared::linkage_context::LinkageContext;
use std::collections::BTreeMap;
use std::sync::Arc;

fn linkage(version: u16) -> LinkageContext {
    let original = AccountAddress::from_suffix(0x2);
    LinkageContext::new(BTreeMap::from([(
        original,
        AccountAddress::from_suffix(version),
    )]))
    .unwrap()
}

fn tag(i: usize) -> TypeTag {
    let mut tag = TypeTag::U8;
    for _ in 0..i % 8 {
        tag = TypeTag::Vector(Box::new(tag));
    }
    tag
}

#[test]
fn buckets_are_by_linkage_and_mode() {
    let bump = Bump::with_capacity(1 << 12);
    let cache = Arc::new(TypeCache::new());
    let v2 = cache.bucket(&bump, true, &linkage(2));
    v2.insert(|b| {
        b.runtime.insert(
            TypeTag::U64,
            Arc::new(move_core_types::runtime_value::MoveTypeLayout::U64),
        );
    });
    let has_u64 = |c: &executor::static_programmable_transactions::type_cache::CachedLinkage| {
        c.get(|b| b.runtime.get(&TypeTag::U64).map(|_| ()))
            .is_some()
    };
    assert!(has_u64(&cache.bucket(&bump, true, &linkage(2))));
    assert!(!has_u64(&cache.bucket(&bump, true, &linkage(3))));
    assert!(!has_u64(&cache.bucket(&bump, false, &linkage(2))));
}

#[test]
fn a_full_cache_starts_over() {
    let bump = Bump::with_capacity(1 << 12);
    let cache = Arc::new(TypeCache::new());
    let held = cache.bucket(&bump, true, &linkage(2));
    for i in 0..MAX_ENTRIES {
        held.insert(|b| {
            b.runtime.insert(
                tag(i),
                Arc::new(move_core_types::runtime_value::MoveTypeLayout::U8),
            );
        });
    }
    // The next bucket handed out is fresh; the one held keeps its entries.
    let fresh = cache.bucket(&bump, true, &linkage(2));
    assert!(fresh.get(|b| b.runtime.get(&tag(0)).map(|_| ())).is_none());
    assert!(held.get(|b| b.runtime.get(&tag(0)).map(|_| ())).is_some());
}
