// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Each `MIN_WIRE_SIZE` must be a lower bound, since a parser refuses a
//! sequence that claims more elements than the remaining bytes could hold.
//! The smallest value of each type is built here and measured.

use anchovy_types::build::base::{
    AccountAddress, Digest, ObjectId, SequenceNumber, SuiAddress, TransactionDigest,
};
use anchovy_types::build::checkpoint::CheckpointTransaction;
use anchovy_types::build::effects::{
    EffectsObjectChange, GasCostSummary, IdOperation, ObjectIn, ObjectOut, TransactionEffects,
    TransactionEffectsV2,
};
use anchovy_types::build::execution_status::ExecutionStatus;
use anchovy_types::build::object::{
    Data, GenesisObject, MoveObject, MoveObjectType, MoveObjectTypeInner, Object, Owner,
};
use anchovy_types::build::signature::{
    CompressedSignature, EmptySignInfo, GenericSignature, ZkLoginAuthenticatorAsBytes,
};
use anchovy_types::build::system_transaction::EndOfEpochTransactionKind;
use anchovy_types::build::transaction::{
    Argument, CallArg, Command, GasData, Intent, IntentMessage, SenderSignedData,
    SenderSignedTransaction, Transaction, TransactionData, TransactionDataV1,
    TransactionExpiration, TransactionKind,
};
use anchovy_types::build::type_tag::TypeTag;
use anchovy_types::{
    Message, Wire, checkpoint, effects, object, signature, system_transaction, transaction,
    type_tag,
};
use serde::Serialize;

fn wire_len<T: Serialize>(value: &T) -> usize {
    bcs::to_bytes(value).unwrap().len()
}

/// The length of the smallest value, which its view parser must also accept.
fn parsed_wire_len<T: Wire, B: Serialize>(value: &B) -> usize {
    let bytes = bcs::to_bytes(value).unwrap();
    let len = bytes.len();
    Message::<T>::parse(bytes).unwrap_or_else(|(e, _)| panic!("{e}"));
    len
}

fn smallest_move_object() -> Data {
    Data::Move(MoveObject {
        type_: MoveObjectType(MoveObjectTypeInner::GasCoin),
        has_public_transfer: false,
        version: SequenceNumber(0),
        contents: Vec::new(),
    })
}

fn smallest_object() -> Object {
    Object {
        data: smallest_move_object(),
        owner: Owner::Immutable,
        previous_transaction: TransactionDigest(Digest([0; 32])),
        storage_rebate: 0,
    }
}

fn smallest_sender_signed_data() -> SenderSignedData {
    SenderSignedData(SenderSignedTransaction {
        intent_message: IntentMessage {
            intent: Intent {
                scope: 0,
                version: 0,
                app_id: 0,
            },
            value: TransactionData::V1(TransactionDataV1 {
                kind: TransactionKind::EndOfEpochTransaction(Vec::new()),
                sender: SuiAddress([0; 32]),
                gas_data: GasData {
                    payment: Vec::new(),
                    owner: SuiAddress([0; 32]),
                    price: 0,
                    budget: 0,
                },
                expiration: TransactionExpiration::None,
            }),
        },
        tx_signatures: Vec::new(),
    })
}

fn smallest_effects() -> TransactionEffects {
    TransactionEffects::V2(Box::new(TransactionEffectsV2 {
        status: ExecutionStatus::Success,
        executed_epoch: 0,
        gas_used: GasCostSummary {
            computation_cost: 0,
            storage_cost: 0,
            storage_rebate: 0,
            non_refundable_storage_fee: 0,
        },
        transaction_digest: TransactionDigest(Digest([0; 32])),
        gas_object_index: None,
        events_digest: None,
        dependencies: Vec::new(),
        lamport_version: SequenceNumber(0),
        changed_objects: Vec::new(),
        unchanged_consensus_objects: Vec::new(),
        aux_data_digest: None,
    }))
}

#[test]
fn type_tag() {
    let len = parsed_wire_len::<type_tag::TypeTag<'static>, _>(&TypeTag::Bool);
    assert_eq!(len, type_tag::TypeTag::MIN_WIRE_SIZE);
}

#[test]
fn argument() {
    assert_eq!(
        wire_len(&Argument::GasCoin),
        transaction::Argument::MIN_WIRE_SIZE
    );
}

#[test]
fn call_arg() {
    assert_eq!(
        wire_len(&CallArg::Pure(Vec::new())),
        transaction::CallArg::MIN_WIRE_SIZE
    );
}

#[test]
fn command() {
    assert_eq!(
        wire_len(&Command::MakeMoveVec(None, Vec::new())),
        transaction::Command::MIN_WIRE_SIZE
    );
}

#[test]
fn generic_signature() {
    assert_eq!(
        wire_len(&GenericSignature(Vec::new())),
        transaction::GenericSignature::MIN_WIRE_SIZE
    );
}

#[test]
fn compressed_signature() {
    let smallest = CompressedSignature::ZkLogin(ZkLoginAuthenticatorAsBytes(Vec::new()));
    assert_eq!(
        wire_len(&smallest),
        signature::CompressedSignature::MIN_WIRE_SIZE
    );
}

#[test]
fn sender_signed_data() {
    let len = parsed_wire_len::<transaction::SenderSignedData<'static>, _>(
        &smallest_sender_signed_data(),
    );
    assert_eq!(len, transaction::SenderSignedData::MIN_WIRE_SIZE);
}

#[test]
fn end_of_epoch_transaction_kind() {
    assert_eq!(
        wire_len(&EndOfEpochTransactionKind::AuthenticatorStateCreate),
        system_transaction::EndOfEpochTransactionKind::MIN_WIRE_SIZE
    );
}

#[test]
fn owner() {
    let len = parsed_wire_len::<object::Owner<'static>, _>(&Owner::Immutable);
    assert_eq!(len, object::Owner::MIN_WIRE_SIZE);
}

#[test]
fn object() {
    let len = parsed_wire_len::<object::Object<'static>, _>(&smallest_object());
    assert_eq!(len, object::Object::MIN_WIRE_SIZE);
}

#[test]
fn genesis_object() {
    let smallest = GenesisObject::RawObject {
        data: smallest_move_object(),
        owner: Owner::Immutable,
    };
    assert_eq!(wire_len(&smallest), object::GenesisObject::MIN_WIRE_SIZE);
}

#[test]
fn object_change() {
    let smallest = (
        ObjectId(AccountAddress([0; 32])),
        EffectsObjectChange {
            input_state: ObjectIn::NotExist,
            output_state: ObjectOut::NotExist,
            id_operation: IdOperation::None,
        },
    );
    assert_eq!(wire_len(&smallest), effects::ObjectChange::MIN_WIRE_SIZE);
}

#[test]
fn transaction_effects() {
    let len = parsed_wire_len::<effects::TransactionEffects<'static>, _>(&smallest_effects());
    assert_eq!(len, effects::TransactionEffects::MIN_WIRE_SIZE);
}

#[test]
fn checkpoint_transaction() {
    let smallest = CheckpointTransaction {
        transaction: Transaction {
            data: smallest_sender_signed_data(),
            auth_signature: EmptySignInfo {},
        },
        effects: smallest_effects(),
        events: None,
        input_objects: Vec::new(),
        output_objects: Vec::new(),
    };
    assert_eq!(
        wire_len(&smallest),
        checkpoint::CheckpointTransaction::MIN_WIRE_SIZE
    );
}
