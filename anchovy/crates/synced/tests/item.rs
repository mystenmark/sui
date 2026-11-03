// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Certified transactions as the executor runs them: the input keys they
//! wait on.

use std::str::FromStr as _;

use messages::Message;
use messages::build::base::{
    AccountAddress, Digest, ObjectDigest, ObjectId as BuildId, SequenceNumber as BuildVersion,
    TransactionDigest,
};
use messages::build::effects::{
    EffectsObjectChange, GasCostSummary, IdOperation, ObjectIn, ObjectOut, TransactionEffects,
    TransactionEffectsV2,
};
use messages::build::execution_status::ExecutionStatus;
use messages::build::object::Owner;
use messages::checkpoint::ExecutionData;
use messages::effects::TransactionEffects as EffectsView;
use messages::transaction::SenderSignedData;
use sui_types::base_types::{ObjectID, SequenceNumber};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    ObjectArg, SharedObjectMutability, TransactionKind, VerifiedTransaction,
};
use sui_types::{Identifier, SUI_ACCUMULATOR_ROOT_OBJECT_ID, SUI_FRAMEWORK_PACKAGE_ID, TypeTag};
use validator::pipeline::keys::InputKey;

const ROOT_START: u64 = 3;
const ROOT_VERSION: u64 = 7;

/// A settlement merging into a `Balance<coin_package::coin::COIN>` account.
fn settlement(coin_package: ObjectID) -> VerifiedTransaction {
    let mut builder = ProgrammableTransactionBuilder::new();
    let root = builder
        .obj(ObjectArg::SharedObject {
            id: SUI_ACCUMULATOR_ROOT_OBJECT_ID,
            initial_shared_version: SequenceNumber::from_u64(ROOT_START),
            mutability: SharedObjectMutability::Mutable,
        })
        .unwrap();
    let args = vec![
        root,
        builder.pure(ObjectID::ZERO).unwrap(),
        builder.pure(1u128).unwrap(),
        builder.pure(0u128).unwrap(),
    ];
    let balance = TypeTag::from_str(&format!(
        "0x2::balance::Balance<{coin_package}::coin::COIN>"
    ))
    .unwrap();
    builder.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("accumulator_settlement").unwrap(),
        Identifier::new("settle_u128").unwrap(),
        vec![balance],
        args,
    );
    VerifiedTransaction::new_system_transaction(TransactionKind::ProgrammableSystemTransaction(
        builder.finish(),
    ))
}

/// Successful effects of `digest` mutating the accumulator root.
fn root_mutated(digest: [u8; 32]) -> Vec<u8> {
    let owner = Owner::Shared {
        initial_shared_version: BuildVersion(ROOT_START),
    };
    let root = BuildId(AccountAddress(SUI_ACCUMULATOR_ROOT_OBJECT_ID.into_bytes()));
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
        dependencies: vec![],
        lamport_version: BuildVersion(ROOT_VERSION + 1),
        changed_objects: vec![(
            root,
            EffectsObjectChange {
                input_state: ObjectIn::Exist((
                    (BuildVersion(ROOT_VERSION), ObjectDigest(Digest([9; 32]))),
                    owner.clone(),
                )),
                output_state: ObjectOut::ObjectWrite((ObjectDigest(Digest([10; 32])), owner)),
                id_operation: IdOperation::None,
            },
        )],
        unchanged_consensus_objects: vec![],
        aux_data_digest: None,
    })))
    .unwrap()
}

/// A settlement waits for the packages its type arguments name, as a user
/// transaction does: one may be published earlier in its own chunk, which
/// reads the root at the same version.
#[test]
fn a_certified_settlement_waits_for_the_packages_it_names() {
    let coin_package = ObjectID::from_single_byte(0xc0);
    let transaction = settlement(coin_package);
    let digest = transaction.digest().into_inner();
    let transaction =
        Message::<SenderSignedData>::parse(bcs::to_bytes(transaction.inner()).unwrap()).unwrap();
    let effects = Message::<EffectsView>::parse(root_mutated(digest)).unwrap();
    let t = ExecutionData {
        transaction: *transaction.get(),
        effects: *effects.get(),
    };
    let (mut keys, mut packages) = (Vec::new(), Vec::new());
    synced::item::executable(
        &t,
        ROOT_VERSION,
        None,
        |_, _, _| unreachable!("no stream ended"),
        &mut keys,
        &mut packages,
    );
    let package = |id: ObjectID| InputKey::Package(messages::base::ObjectId(id.into_bytes()));
    assert!(keys.contains(&package(coin_package)), "{keys:?}");
    assert!(
        keys.contains(&package(SUI_FRAMEWORK_PACKAGE_ID)),
        "{keys:?}"
    );
    assert!(keys.contains(&InputKey::ObjectVersion {
        id: messages::base::ObjectId(SUI_ACCUMULATOR_ROOT_OBJECT_ID.into_bytes()),
        version: ROOT_VERSION,
    }));
}
