// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Address-balance object ids: dynamic fields of the accumulator root.

use blake2::Blake2b;
use blake2::digest::consts::U32;
use messages::base::{ObjectId, SuiAddress};

/// The accumulator root object, `0xacc`.
const ACCUMULATOR_ROOT: [u8; 32] = {
    let mut a = [0; 32];
    a[30] = 0x0a;
    a[31] = 0xcc;
    a
};

/// `HashingIntentScope::ChildObjectId`.
const CHILD_OBJECT_ID_SCOPE: u8 = 0xf0;

/// BCS of the type tag
/// `0x2::accumulator::Key<0x2::balance::Balance<0x2::sui::SUI>>`.
fn sui_balance_key_type_tag() -> Vec<u8> {
    fn framework() -> [u8; 32] {
        let mut a = [0; 32];
        a[31] = 2;
        a
    }
    fn ident(out: &mut Vec<u8>, s: &str) {
        out.push(s.len() as u8);
        out.extend_from_slice(s.as_bytes());
    }
    // `TypeTag::Struct` is variant 7; a struct tag is address, module,
    // name, then its type parameters.
    let mut out = Vec::with_capacity(128);
    for (module, name) in [("accumulator", "Key"), ("balance", "Balance")] {
        out.push(7);
        out.extend_from_slice(&framework());
        ident(&mut out, module);
        ident(&mut out, name);
        out.push(1);
    }
    out.push(7);
    out.extend_from_slice(&framework());
    ident(&mut out, "sui");
    ident(&mut out, "SUI");
    out.push(0);
    out
}

/// The id of `owner`'s SUI address balance, as the reference's
/// `AccumulatorValue::get_field_id`: the dynamic field of the accumulator
/// root keyed by `Key<Balance<SUI>> { owner }`.
pub fn sui_balance_id(owner: &SuiAddress) -> ObjectId {
    use blake2::Digest as _;
    // hash(scope || parent || len(key) || key || key type tag), the key
    // being the owner's address and its length a little-endian usize.
    let mut hasher = Blake2b::<U32>::new();
    hasher.update([CHILD_OBJECT_ID_SCOPE]);
    hasher.update(ACCUMULATOR_ROOT);
    hasher.update(32u64.to_le_bytes());
    hasher.update(owner.0);
    hasher.update(sui_balance_key_type_tag());
    ObjectId(hasher.finalize().into())
}
