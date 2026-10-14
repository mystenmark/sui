// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Effects with accumulator writes, built with the wire mirrors.

#![allow(dead_code)]

use messages::build::base::{
    AccountAddress, Digest, ObjectDigest, ObjectId, SequenceNumber, SuiAddress, TransactionDigest,
};
use messages::build::effects::{
    AccumulatorAddress, AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1,
    EffectsObjectChange, GasCostSummary, IdOperation, ObjectIn, ObjectOut, TransactionEffects,
    TransactionEffectsV2,
};
use messages::build::execution_status::ExecutionStatus;
use messages::build::object::Owner;
use messages::build::type_tag::{StructTag, TypeTag};

pub const ROOT: [u8; 32] = id(0x0acc);

pub const fn id(n: u16) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[30] = (n >> 8) as u8;
    bytes[31] = n as u8;
    bytes
}

pub fn digest(n: u64) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    bytes[31] = 0xd1;
    bytes
}

fn struct_tag(address: u16, module: &str, name: &str, type_params: Vec<TypeTag>) -> TypeTag {
    TypeTag::Struct(Box::new(StructTag {
        address: AccountAddress(id(address)),
        module: module.to_owned(),
        name: name.to_owned(),
        type_params,
    }))
}

/// `Balance<address::module::NAME>`.
pub fn balance_of(address: u16, module: &str, name: &str) -> TypeTag {
    struct_tag(
        2,
        "balance",
        "Balance",
        vec![struct_tag(address, module, name, vec![])],
    )
}

pub fn sui_balance() -> TypeTag {
    balance_of(2, "sui", "SUI")
}

/// The type of an event stream's accumulator.
pub fn event_stream() -> TypeTag {
    struct_tag(2, "accumulator_settlement", "EventStreamHead", vec![])
}

/// The accumulator object of `owner`'s balance of type `ty`, as the
/// reference derives it.
pub fn account_id(owner: [u8; 32], ty: &TypeTag) -> [u8; 32] {
    let ty: sui_types::TypeTag = bcs::from_bytes(&bcs::to_bytes(ty).unwrap()).unwrap();
    let owner = sui_types::base_types::SuiAddress::from_bytes(owner).unwrap();
    sui_types::accumulator_root::AccumulatorValue::get_field_id(owner, &ty)
        .unwrap()
        .inner()
        .into_bytes()
}

/// A write to accumulator object `account`, owned by `owner`.
pub fn write(
    account: [u8; 32],
    owner: [u8; 32],
    ty: TypeTag,
    operation: AccumulatorOperation,
    value: AccumulatorValue,
) -> (ObjectId, EffectsObjectChange) {
    (
        ObjectId(AccountAddress(account)),
        EffectsObjectChange {
            input_state: ObjectIn::NotExist,
            output_state: ObjectOut::AccumulatorWriteV1(AccumulatorWriteV1 {
                address: AccumulatorAddress {
                    address: SuiAddress(owner),
                    ty,
                },
                operation,
                value,
            }),
            id_operation: IdOperation::None,
        },
    )
}

/// A deposit of `amount` into `owner`'s balance of type `ty`.
pub fn merge(owner: [u8; 32], ty: TypeTag, amount: u64) -> (ObjectId, EffectsObjectChange) {
    let account = account_id(owner, &ty);
    write(
        account,
        owner,
        ty,
        AccumulatorOperation::Merge,
        AccumulatorValue::Integer(amount),
    )
}

/// A withdrawal of `amount` from `owner`'s balance of type `ty`.
pub fn split(owner: [u8; 32], ty: TypeTag, amount: u64) -> (ObjectId, EffectsObjectChange) {
    let account = account_id(owner, &ty);
    write(
        account,
        owner,
        ty,
        AccumulatorOperation::Split,
        AccumulatorValue::Integer(amount),
    )
}

pub fn events(
    stream: [u8; 32],
    owner: [u8; 32],
    commitments: &[(u64, u64)],
) -> (ObjectId, EffectsObjectChange) {
    write(
        stream,
        owner,
        event_stream(),
        AccumulatorOperation::Merge,
        AccumulatorValue::EventDigest(
            commitments
                .iter()
                .map(|&(index, d)| (index, Digest(digest(d))))
                .collect(),
        ),
    )
}

/// An object `id` the transaction created (`created`) or deleted.
pub fn created_or_deleted(object: [u8; 32], created: bool) -> (ObjectId, EffectsObjectChange) {
    let owner = Owner::ObjectOwner(SuiAddress(ROOT));
    (
        ObjectId(AccountAddress(object)),
        if created {
            EffectsObjectChange {
                input_state: ObjectIn::NotExist,
                output_state: ObjectOut::ObjectWrite((ObjectDigest(Digest([7; 32])), owner)),
                id_operation: IdOperation::Created,
            }
        } else {
            EffectsObjectChange {
                input_state: ObjectIn::Exist((
                    (SequenceNumber(3), ObjectDigest(Digest([8; 32]))),
                    owner,
                )),
                output_state: ObjectOut::NotExist,
                id_operation: IdOperation::Deleted,
            }
        },
    )
}

/// The accumulator root, shared at `initial_shared_version`, mutated from
/// `version`.
pub fn root_mutated(initial_shared_version: u64, version: u64) -> (ObjectId, EffectsObjectChange) {
    let owner = Owner::Shared {
        initial_shared_version: SequenceNumber(initial_shared_version),
    };
    (
        ObjectId(AccountAddress(ROOT)),
        EffectsObjectChange {
            input_state: ObjectIn::Exist((
                (SequenceNumber(version), ObjectDigest(Digest([9; 32]))),
                owner.clone(),
            )),
            output_state: ObjectOut::ObjectWrite((ObjectDigest(Digest([10; 32])), owner)),
            id_operation: IdOperation::None,
        },
    )
}

/// Successful V2 effects of transaction `digest`, as BCS.
pub fn effects(
    digest: [u8; 32],
    lamport_version: u64,
    dependencies: &[[u8; 32]],
    mut changed_objects: Vec<(ObjectId, EffectsObjectChange)>,
) -> Vec<u8> {
    changed_objects.sort_by_key(|(id, _)| *id);
    bcs::to_bytes(&TransactionEffects::V2(Box::new(TransactionEffectsV2 {
        status: ExecutionStatus::Success,
        executed_epoch: 0,
        gas_used: GasCostSummary {
            computation_cost: 0,
            storage_cost: 0,
            storage_rebate: 0,
            non_refundable_storage_fee: 0,
        },
        transaction_digest: TransactionDigest(Digest(digest)),
        gas_object_index: None,
        events_digest: None,
        dependencies: dependencies
            .iter()
            .map(|d| TransactionDigest(Digest(*d)))
            .collect(),
        lamport_version: SequenceNumber(lamport_version),
        changed_objects,
        unchanged_consensus_objects: vec![],
        aux_data_digest: None,
    })))
    .unwrap()
}

/// A fixed chunk: SUI and another coin deposited and withdrawn, a net-zero
/// account, an event stream fed by two transactions, and a transaction
/// with no accumulator writes.
pub fn fixed_chunk() -> Vec<Vec<u8>> {
    let usdc = || balance_of(5, "usdc", "USDC");
    let (a, b, c) = (id(0xa1), id(0xb1), id(0xc1));
    let stream = id(0x1005);
    vec![
        effects(
            digest(1),
            5,
            &[],
            vec![
                merge(a, sui_balance(), 100),
                events(stream, c, &[(0, 11), (2, 12)]),
            ],
        ),
        effects(digest(2), 5, &[], vec![]),
        effects(
            digest(3),
            5,
            &[],
            vec![
                split(a, sui_balance(), 30),
                merge(b, sui_balance(), 50),
                merge(a, usdc(), 7),
            ],
        ),
        effects(
            digest(4),
            5,
            &[],
            vec![split(b, sui_balance(), 50), events(stream, c, &[(1, 13)])],
        ),
        effects(digest(5), 5, &[], vec![split(c, usdc(), 9)]),
    ]
}

/// The accumulator root object, shared at `initial_shared_version`, as BCS.
#[allow(dead_code)]
pub fn root_object(initial_shared_version: u64) -> Vec<u8> {
    use messages::build::base::{Digest as D, SequenceNumber, TransactionDigest};
    use messages::build::object::{Data, MoveObject, MoveObjectType, MoveObjectTypeInner, Object};
    bcs::to_bytes(&Object {
        data: Data::Move(MoveObject {
            type_: MoveObjectType(MoveObjectTypeInner::SuiBalanceAccumulatorField),
            has_public_transfer: false,
            version: SequenceNumber(initial_shared_version),
            contents: vec![0; 32],
        }),
        owner: messages::build::object::Owner::Shared {
            initial_shared_version: SequenceNumber(initial_shared_version),
        },
        previous_transaction: TransactionDigest(D([0; 32])),
        storage_rebate: 0,
    })
    .unwrap()
}

/// The balance accounts `fixed_chunk` writes, in id order.
#[allow(dead_code)]
pub fn fixed_chunk_accounts() -> Vec<messages::base::ObjectId> {
    let usdc = balance_of(5, "usdc", "USDC");
    let mut accounts: Vec<messages::base::ObjectId> = [
        account_id(id(0xa1), &sui_balance()),
        account_id(id(0xb1), &sui_balance()),
        account_id(id(0xa1), &usdc),
        account_id(id(0xc1), &usdc),
    ]
    .into_iter()
    .map(messages::base::ObjectId)
    .collect();
    accounts.sort_unstable();
    accounts
}

/// `count` transactions each moving SUI between two of `accounts`
/// accounts, as a chunk under load.
pub fn transfers(count: u64, accounts: u64) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let from = (i * 7) % accounts;
            let mut to = (i * 13 + 1) % accounts;
            if to == from {
                to = (to + 1) % accounts;
            }
            let owner = |n: u64| id(0x8000 + n as u16);
            effects(
                digest(1000 + i),
                5,
                &[],
                vec![
                    split(owner(from), sui_balance(), 10 + i),
                    merge(owner(to), sui_balance(), 10 + i),
                ],
            )
        })
        .collect()
}
