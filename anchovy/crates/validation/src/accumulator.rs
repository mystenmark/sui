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

/// Feeds BCS of the type tag
/// `0x2::accumulator::Key<0x2::balance::Balance<0x2::sui::SUI>>` to `hasher`.
fn hash_sui_balance_key_type_tag(hasher: &mut Blake2b<U32>) {
    use blake2::Digest as _;
    let mut framework = [0u8; 32];
    framework[31] = 2;
    let ident = |hasher: &mut Blake2b<U32>, s: &str| {
        hasher.update([s.len() as u8]);
        hasher.update(s.as_bytes());
    };
    // `TypeTag::Struct` is variant 7; a struct tag is address, module,
    // name, then its type parameters.
    for (module, name) in [("accumulator", "Key"), ("balance", "Balance")] {
        hasher.update([7]);
        hasher.update(framework);
        ident(hasher, module);
        ident(hasher, name);
        hasher.update([1]);
    }
    hasher.update([7]);
    hasher.update(framework);
    ident(hasher, "sui");
    ident(hasher, "SUI");
    hasher.update([0]);
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
    hash_sui_balance_key_type_tag(&mut hasher);
    ObjectId(hasher.finalize().into())
}
