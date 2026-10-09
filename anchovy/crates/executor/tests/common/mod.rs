// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Random sui-types values and their conversions to the port's types, shared by the
//! differential tests.

#![allow(dead_code)]

use std::collections::BTreeMap;

use containers::Bump;
use messages::arena::BumpAlloc;
use messages::base::{Digest, ObjectId};
use messages::reader::Reader;
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, TransactionDigest};
use sui_types::digests::ObjectDigest;
use sui_types::effects::AccumulatorWriteV1;
use sui_types::execution_status::ExecutionStatus;
use sui_types::move_package::{MovePackage, TypeOrigin, UpgradeInfo};
use sui_types::object::{MoveObject, Object, Owner};

/// xorshift64*: deterministic, no dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn bytes(&mut self) -> [u8; 32] {
        let mut b = [0; 32];
        for chunk in b.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes());
        }
        b
    }

    pub fn id(&mut self) -> ObjectID {
        ObjectID::new(self.bytes())
    }

    pub fn address(&mut self) -> SuiAddress {
        SuiAddress::from(self.id())
    }

    pub fn tx_digest(&mut self) -> TransactionDigest {
        TransactionDigest::new(self.bytes())
    }

    pub fn object_digest(&mut self) -> ObjectDigest {
        ObjectDigest::new(self.bytes())
    }

    /// An owner that is not `Shared`; `Immutable` only if `immutable`.
    pub fn unshared_owner(&mut self, immutable: bool) -> Owner {
        match self.below(if immutable { 4 } else { 3 }) {
            0 => Owner::AddressOwner(self.address()),
            1 => Owner::ObjectOwner(self.address()),
            2 => Owner::ConsensusAddressOwner {
                start_version: SequenceNumber::from(self.below(100)),
                owner: self.address(),
            },
            _ => Owner::Immutable,
        }
    }

    pub fn shared_owner(&mut self) -> Owner {
        Owner::Shared {
            initial_shared_version: SequenceNumber::from(1 + self.below(100)),
        }
    }
}

// Conversions to the port's types, through BCS.

pub fn reader<'a>(bump: &'a Bump, value: &impl serde::Serialize) -> Reader<'a> {
    let bytes = bcs::to_bytes(value).unwrap();
    Reader::new(containers::alloc_slice_copy(bump, &bytes))
}

pub fn oid(id: ObjectID) -> ObjectId {
    ObjectId(id.into_bytes())
}

pub fn digest(d: impl AsRef<[u8]>) -> Digest {
    Digest::new(d.as_ref().try_into().unwrap())
}

pub fn owner<'a>(bump: &'a Bump, o: &Owner) -> messages::object::Owner<'a> {
    messages::object::Owner::parse(&mut reader(bump, o), &mut BumpAlloc(bump)).unwrap()
}

pub fn status<'a>(
    bump: &'a Bump,
    s: &ExecutionStatus,
) -> messages::execution_status::ExecutionStatus<'a> {
    messages::execution_status::ExecutionStatus::parse(&mut reader(bump, s)).unwrap()
}

pub fn accumulator_write<'a>(
    bump: &'a Bump,
    w: &AccumulatorWriteV1,
) -> messages::effects::AccumulatorWriteV1<'a> {
    messages::effects::AccumulatorWriteV1::parse(&mut reader(bump, w), &mut BumpAlloc(bump))
        .unwrap()
}

/// The object as the store holds it, with its stored bytes.
pub fn stored_object<'a>(bump: &'a Bump, o: &Object) -> exec_types::object::Object<'a> {
    let view = messages::object::Object::parse(&mut reader(bump, o), &mut BumpAlloc(bump)).unwrap();
    exec_types::object::Object::from_view(&view)
}

/// The object rebuilt from its parts and sealed, as execution writes it.
pub fn sealed_object<'a>(bump: &'a Bump, o: &Object) -> exec_types::object::Object<'a> {
    let stored = stored_object(bump, o);
    let rebuilt = match stored.data() {
        messages::object::Data::Move(m) => {
            exec_types::object::Object::new_move(*m, *stored.owner(), stored.previous_transaction())
        }
        messages::object::Data::Package(p) => {
            exec_types::object::Object::new_from_package(*p, stored.previous_transaction())
        }
    }
    .with_storage_rebate(stored.storage_rebate());
    assert!(rebuilt.stored_bytes().is_none());
    let sealed = rebuilt.seal(bump);
    assert_eq!(sealed.stored_bytes(), stored.stored_bytes());
    sealed
}

pub fn coin(rng: &mut Rng, id: ObjectID, version: u64, owner: Owner) -> Object {
    let o = MoveObject::new_gas_coin(SequenceNumber::from(version), id, rng.below(1 << 40));
    Object::new_move(o, owner, rng.tx_digest())
}

pub fn package(rng: &mut Rng, id: ObjectID, version: u64) -> Object {
    let mut modules = BTreeMap::new();
    let len = 1 + rng.below(64) as usize;
    modules.insert(
        "m".to_string(),
        (0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>(),
    );
    let origins = vec![TypeOrigin {
        module_name: "m".into(),
        datatype_name: "S".into(),
        package: id,
    }];
    let mut linkage = BTreeMap::new();
    linkage.insert(
        ObjectID::from_single_byte(2),
        UpgradeInfo {
            upgraded_id: ObjectID::from_single_byte(2),
            upgraded_version: SequenceNumber::from(1),
        },
    );
    let bytes =
        bcs::to_bytes(&(id, SequenceNumber::from(version), modules, origins, linkage)).unwrap();
    let pkg: MovePackage = bcs::from_bytes(&bytes).unwrap();
    Object::new_from_package(pkg, rng.tx_digest())
}
