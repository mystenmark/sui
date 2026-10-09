// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::base_types` and the system ids of `sui_types`' root.

use blake2::Blake2b;
use blake2::digest::Digest as _;
use blake2::digest::consts::U32;
use messages::base::{AccountAddress, ObjectDigest, ObjectId, SequenceNumber, TransactionDigest};

pub type ObjectRef = (ObjectId, SequenceNumber, ObjectDigest);

pub type EpochId = u64;

pub const MOVE_STDLIB_ADDRESS: AccountAddress = address(0x1);
pub const SUI_FRAMEWORK_ADDRESS: AccountAddress = address(0x2);
pub const SUI_SYSTEM_ADDRESS: AccountAddress = address(0x3);

pub const SUI_SYSTEM_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x5);
pub const SUI_CLOCK_OBJECT_ID: ObjectId = ObjectId::from_u16(0x6);
pub const SUI_AUTHENTICATOR_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x7);
pub const SUI_RANDOMNESS_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x8);
pub const SUI_BRIDGE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x9);
pub const SUI_ADDRESS_ALIAS_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0xa);
pub const SUI_COIN_REGISTRY_OBJECT_ID: ObjectId = ObjectId::from_u16(0xc);
pub const SUI_DISPLAY_REGISTRY_OBJECT_ID: ObjectId = ObjectId::from_u16(0xd);
pub const SUI_FORWARDING_ADDRESS_REGISTRY_OBJECT_ID: ObjectId = ObjectId::from_u16(0xfa);
pub const SUI_DENY_LIST_OBJECT_ID: ObjectId = ObjectId::from_u16(0x403);
pub const SUI_ACCUMULATOR_ROOT_OBJECT_ID: ObjectId = ObjectId::from_u16(0xacc);

const fn address(n: u16) -> AccountAddress {
    AccountAddress(ObjectId::from_u16(n).0)
}

/// `shared_crypto::intent::HashingIntentScope`.
#[repr(u8)]
pub enum HashingIntentScope {
    ChildObjectId = 0xf0,
    RegularObjectId = 0xf1,
}

/// `ObjectID::derive_id`: the id of the `creation_num`th object a
/// transaction creates.
pub fn derive_id(digest: &TransactionDigest, creation_num: u64) -> ObjectId {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update([HashingIntentScope::RegularObjectId as u8]);
    hasher.update(digest.bytes);
    hasher.update(creation_num.to_le_bytes());
    ObjectId(hasher.finalize().into())
}

pub fn object_id(address: &move_core_types::account_address::AccountAddress) -> ObjectId {
    ObjectId(address.into_bytes())
}

pub fn move_address(id: &ObjectId) -> move_core_types::account_address::AccountAddress {
    move_core_types::account_address::AccountAddress::new(id.0)
}
