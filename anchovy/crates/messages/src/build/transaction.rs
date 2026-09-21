// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{ChainIdentifier, ObjectId, ObjectRef, SequenceNumber, SuiAddress};
use super::object::GenesisObject;
use super::signature::{EmptySignInfo, GenericSignature};
use super::system_transaction::{
    AuthenticatorStateUpdate, ChangeEpoch, ConsensusCommitPrologue, ConsensusCommitPrologueV2,
    ConsensusCommitPrologueV3, ConsensusCommitPrologueV4, EndOfEpochTransactionKind,
    GenesisTransaction, RandomnessStateUpdate,
};
use super::type_tag::{TypeInput, TypeTag};
use crate::transaction as view;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument {
    GasCoin,
    Input(u16),
    Result(u16),
    NestedResult(u16, u16),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedObjectMutability {
    Immutable,
    Mutable,
    NonExclusiveWrite,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectArg {
    ImmOrOwnedObject(ObjectRef),
    SharedObject {
        id: ObjectId,
        initial_shared_version: SequenceNumber,
        mutability: SharedObjectMutability,
    },
    Receiving(ObjectRef),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reservation {
    MaxAmountU64(u64),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WithdrawalTypeArg {
    Balance(TypeTag),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawFrom {
    Sender,
    Sponsor,
    SenderAllowance {
        funder: SuiAddress,
        allowance: ObjectId,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FundsWithdrawalArg {
    pub reservation: Reservation,
    pub type_arg: WithdrawalTypeArg,
    pub withdraw_from: WithdrawFrom,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CallArg {
    Pure(Vec<u8>),
    Object(ObjectArg),
    FundsWithdrawal(FundsWithdrawalArg),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProgrammableMoveCall {
    pub package: ObjectId,
    pub module: String,
    pub function: String,
    pub type_arguments: Vec<TypeInput>,
    pub arguments: Vec<Argument>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Command {
    MoveCall(Box<ProgrammableMoveCall>),
    TransferObjects(Vec<Argument>, Argument),
    SplitCoins(Argument, Vec<Argument>),
    MergeCoins(Argument, Vec<Argument>),
    /// Modules, then dependencies.
    Publish(Vec<Vec<u8>>, Vec<ObjectId>),
    MakeMoveVec(Option<TypeInput>, Vec<Argument>),
    /// Modules, dependencies, the package being upgraded, the upgrade ticket.
    Upgrade(Vec<Vec<u8>>, Vec<ObjectId>, ObjectId, Argument),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProgrammableTransaction {
    pub inputs: Vec<CallArg>,
    pub commands: Vec<Command>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransactionKind {
    ProgrammableTransaction(ProgrammableTransaction),
    ChangeEpoch(ChangeEpoch),
    Genesis(GenesisTransaction),
    ConsensusCommitPrologue(ConsensusCommitPrologue),
    AuthenticatorStateUpdate(AuthenticatorStateUpdate),
    EndOfEpochTransaction(Vec<EndOfEpochTransactionKind>),
    RandomnessStateUpdate(RandomnessStateUpdate),
    ConsensusCommitPrologueV2(ConsensusCommitPrologueV2),
    ConsensusCommitPrologueV3(ConsensusCommitPrologueV3),
    ConsensusCommitPrologueV4(ConsensusCommitPrologueV4),
    ProgrammableSystemTransaction(ProgrammableTransaction),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GasData {
    pub payment: Vec<ObjectRef>,
    pub owner: SuiAddress,
    pub price: u64,
    pub budget: u64,
}

/// `proposers` may be empty here; the reference rejects that.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AllowedProposers {
    pub epoch: u64,
    pub proposers: Vec<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransactionExpiration {
    None,
    Epoch(u64),
    ValidDuring {
        min_epoch: Option<u64>,
        max_epoch: Option<u64>,
        min_timestamp: Option<u64>,
        max_timestamp: Option<u64>,
        chain: ChainIdentifier,
        nonce: u32,
    },
    Validity {
        min_epoch: Option<u64>,
        max_epoch: Option<u64>,
        min_timestamp: Option<u64>,
        max_timestamp: Option<u64>,
        chain: ChainIdentifier,
        nonce: u32,
        allowed_proposers: Option<AllowedProposers>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionDataV1 {
    pub kind: TransactionKind,
    pub sender: SuiAddress,
    pub gas_data: GasData,
    pub expiration: TransactionExpiration,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransactionData {
    V1(TransactionDataV1),
}

/// The values are not checked against the known scopes, versions and apps.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intent {
    pub scope: u8,
    pub version: u8,
    pub app_id: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IntentMessage {
    pub intent: Intent,
    pub value: TransactionData,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SenderSignedTransaction {
    pub intent_message: IntentMessage,
    pub tx_signatures: Vec<GenericSignature>,
}

/// On the wire a sequence, which must hold exactly one transaction.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SenderSignedData(#[serde(with = "super::one_element")] pub SenderSignedTransaction);

/// `Envelope<SenderSignedData, EmptySignInfo>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(
    rename = "sui_types::message_envelope::Envelope<sui_types::transaction::SenderSignedData, sui_types::crypto::EmptySignInfo>"
)]
pub struct Transaction {
    pub data: SenderSignedData,
    pub auth_signature: EmptySignInfo,
}

impl From<&view::Argument> for Argument {
    fn from(v: &view::Argument) -> Self {
        match *v {
            view::Argument::GasCoin => Argument::GasCoin,
            view::Argument::Input(i) => Argument::Input(i),
            view::Argument::Result(i) => Argument::Result(i),
            view::Argument::NestedResult(i, j) => Argument::NestedResult(i, j),
        }
    }
}

impl From<&view::SharedObjectMutability> for SharedObjectMutability {
    fn from(v: &view::SharedObjectMutability) -> Self {
        match v {
            view::SharedObjectMutability::Immutable => SharedObjectMutability::Immutable,
            view::SharedObjectMutability::Mutable => SharedObjectMutability::Mutable,
            view::SharedObjectMutability::NonExclusiveWrite => {
                SharedObjectMutability::NonExclusiveWrite
            }
        }
    }
}

impl From<&view::ObjectArg<'_>> for ObjectArg {
    fn from(v: &view::ObjectArg<'_>) -> Self {
        match v {
            view::ObjectArg::ImmOrOwnedObject(object) => {
                ObjectArg::ImmOrOwnedObject(ObjectRef::from(*object))
            }
            view::ObjectArg::SharedObject(shared) => ObjectArg::SharedObject {
                id: ObjectId::from(&shared.id),
                initial_shared_version: SequenceNumber(shared.initial_shared_version.get()),
                mutability: SharedObjectMutability::from(&shared.mutability()),
            },
            view::ObjectArg::Receiving(object) => ObjectArg::Receiving(ObjectRef::from(*object)),
        }
    }
}

impl From<&view::Reservation> for Reservation {
    fn from(v: &view::Reservation) -> Self {
        match *v {
            view::Reservation::MaxAmountU64(amount) => Reservation::MaxAmountU64(amount),
        }
    }
}

impl From<&view::WithdrawalTypeArg<'_>> for WithdrawalTypeArg {
    fn from(v: &view::WithdrawalTypeArg<'_>) -> Self {
        match v {
            view::WithdrawalTypeArg::Balance(tag) => WithdrawalTypeArg::Balance(TypeTag::from(tag)),
        }
    }
}

impl From<&view::WithdrawFrom<'_>> for WithdrawFrom {
    fn from(v: &view::WithdrawFrom<'_>) -> Self {
        match v {
            view::WithdrawFrom::Sender => WithdrawFrom::Sender,
            view::WithdrawFrom::Sponsor => WithdrawFrom::Sponsor,
            view::WithdrawFrom::SenderAllowance { funder, allowance } => {
                WithdrawFrom::SenderAllowance {
                    funder: SuiAddress::from(*funder),
                    allowance: ObjectId::from(*allowance),
                }
            }
        }
    }
}

impl From<&view::FundsWithdrawalArg<'_>> for FundsWithdrawalArg {
    fn from(v: &view::FundsWithdrawalArg<'_>) -> Self {
        FundsWithdrawalArg {
            reservation: Reservation::from(&v.reservation),
            type_arg: WithdrawalTypeArg::from(&v.type_arg),
            withdraw_from: WithdrawFrom::from(&v.withdraw_from),
        }
    }
}

impl From<&view::CallArg<'_>> for CallArg {
    fn from(v: &view::CallArg<'_>) -> Self {
        match v {
            view::CallArg::Pure(bytes) => CallArg::Pure(bytes.to_vec()),
            view::CallArg::Object(object) => CallArg::Object(ObjectArg::from(object)),
            view::CallArg::FundsWithdrawal(withdrawal) => {
                CallArg::FundsWithdrawal(FundsWithdrawalArg::from(&**withdrawal))
            }
        }
    }
}

impl From<&view::ProgrammableMoveCall<'_>> for ProgrammableMoveCall {
    fn from(v: &view::ProgrammableMoveCall<'_>) -> Self {
        ProgrammableMoveCall {
            package: ObjectId::from(v.package),
            module: v.module.to_owned(),
            function: v.function.to_owned(),
            type_arguments: v.type_arguments.iter().map(TypeInput::from).collect(),
            arguments: v.arguments.iter().map(Argument::from).collect(),
        }
    }
}

impl From<&view::Command<'_>> for Command {
    fn from(v: &view::Command<'_>) -> Self {
        match v {
            view::Command::MoveCall(call) => {
                Command::MoveCall(Box::new(ProgrammableMoveCall::from(call)))
            }
            view::Command::TransferObjects(objects, recipient) => Command::TransferObjects(
                objects.iter().map(Argument::from).collect(),
                Argument::from(recipient),
            ),
            view::Command::SplitCoins(coin, amounts) => Command::SplitCoins(
                Argument::from(coin),
                amounts.iter().map(Argument::from).collect(),
            ),
            view::Command::MergeCoins(coin, sources) => Command::MergeCoins(
                Argument::from(coin),
                sources.iter().map(Argument::from).collect(),
            ),
            view::Command::Publish(modules, dependencies) => Command::Publish(
                modules.iter().map(|module| module.to_vec()).collect(),
                dependencies.iter().map(ObjectId::from).collect(),
            ),
            view::Command::MakeMoveVec(ty, elements) => Command::MakeMoveVec(
                ty.as_ref().map(TypeInput::from),
                elements.iter().map(Argument::from).collect(),
            ),
            view::Command::Upgrade(modules, dependencies, package, ticket) => Command::Upgrade(
                modules.iter().map(|module| module.to_vec()).collect(),
                dependencies.iter().map(ObjectId::from).collect(),
                ObjectId::from(*package),
                Argument::from(ticket),
            ),
        }
    }
}

impl From<&view::ProgrammableTransaction<'_>> for ProgrammableTransaction {
    fn from(v: &view::ProgrammableTransaction<'_>) -> Self {
        ProgrammableTransaction {
            inputs: v.inputs.iter().map(CallArg::from).collect(),
            commands: v.commands.iter().map(Command::from).collect(),
        }
    }
}

impl From<&view::TransactionKind<'_>> for TransactionKind {
    fn from(v: &view::TransactionKind<'_>) -> Self {
        use TransactionKind as B;
        use view::TransactionKind as V;
        match v {
            V::ProgrammableTransaction(pt) => {
                B::ProgrammableTransaction(ProgrammableTransaction::from(pt))
            }
            V::ChangeEpoch(change) => B::ChangeEpoch(ChangeEpoch::from(&**change)),
            V::Genesis(objects) => B::Genesis(GenesisTransaction {
                objects: objects.iter().map(GenesisObject::from).collect(),
            }),
            V::ConsensusCommitPrologue(prologue) => {
                B::ConsensusCommitPrologue(ConsensusCommitPrologue::from(prologue))
            }
            V::AuthenticatorStateUpdate(update) => {
                B::AuthenticatorStateUpdate(AuthenticatorStateUpdate::from(&**update))
            }
            V::EndOfEpochTransaction(kinds) => B::EndOfEpochTransaction(
                kinds.iter().map(EndOfEpochTransactionKind::from).collect(),
            ),
            V::RandomnessStateUpdate(update) => {
                B::RandomnessStateUpdate(RandomnessStateUpdate::from(&**update))
            }
            V::ConsensusCommitPrologueV2(prologue) => {
                B::ConsensusCommitPrologueV2(ConsensusCommitPrologueV2::from(prologue))
            }
            V::ConsensusCommitPrologueV3(prologue) => {
                B::ConsensusCommitPrologueV3(ConsensusCommitPrologueV3::from(&**prologue))
            }
            V::ConsensusCommitPrologueV4(prologue) => {
                B::ConsensusCommitPrologueV4(ConsensusCommitPrologueV4::from(&**prologue))
            }
            V::ProgrammableSystemTransaction(pt) => {
                B::ProgrammableSystemTransaction(ProgrammableTransaction::from(pt))
            }
        }
    }
}

impl From<&view::GasData<'_>> for GasData {
    fn from(v: &view::GasData<'_>) -> Self {
        GasData {
            payment: v.payment.iter().map(ObjectRef::from).collect(),
            owner: SuiAddress::from(v.owner),
            price: v.price,
            budget: v.budget,
        }
    }
}

impl From<&view::AllowedProposers<'_>> for AllowedProposers {
    fn from(v: &view::AllowedProposers<'_>) -> Self {
        AllowedProposers {
            epoch: v.epoch,
            proposers: v.proposers.iter().map(|p| p.get()).collect(),
        }
    }
}

impl From<&view::TransactionExpiration<'_>> for TransactionExpiration {
    fn from(v: &view::TransactionExpiration<'_>) -> Self {
        match v {
            view::TransactionExpiration::None => TransactionExpiration::None,
            view::TransactionExpiration::Epoch(epoch) => TransactionExpiration::Epoch(*epoch),
            view::TransactionExpiration::ValidDuring(window) => {
                TransactionExpiration::ValidDuring {
                    min_epoch: window.min_epoch,
                    max_epoch: window.max_epoch,
                    min_timestamp: window.min_timestamp,
                    max_timestamp: window.max_timestamp,
                    chain: ChainIdentifier::from(window.chain),
                    nonce: window.nonce,
                }
            }
            view::TransactionExpiration::Validity(window, allowed_proposers) => {
                TransactionExpiration::Validity {
                    min_epoch: window.min_epoch,
                    max_epoch: window.max_epoch,
                    min_timestamp: window.min_timestamp,
                    max_timestamp: window.max_timestamp,
                    chain: ChainIdentifier::from(window.chain),
                    nonce: window.nonce,
                    allowed_proposers: allowed_proposers.as_ref().map(AllowedProposers::from),
                }
            }
        }
    }
}

impl From<&view::TransactionData<'_>> for TransactionDataV1 {
    fn from(v: &view::TransactionData<'_>) -> Self {
        TransactionDataV1 {
            kind: TransactionKind::from(&v.kind),
            sender: SuiAddress::from(v.sender),
            gas_data: GasData::from(&v.gas_data),
            expiration: TransactionExpiration::from(&v.expiration),
        }
    }
}

impl From<&view::TransactionData<'_>> for TransactionData {
    fn from(v: &view::TransactionData<'_>) -> Self {
        TransactionData::V1(TransactionDataV1::from(v))
    }
}

impl From<&view::Intent> for Intent {
    fn from(v: &view::Intent) -> Self {
        Intent {
            scope: v.scope,
            version: v.version,
            app_id: v.app_id,
        }
    }
}

impl From<&view::SenderSignedData<'_>> for SenderSignedData {
    fn from(v: &view::SenderSignedData<'_>) -> Self {
        SenderSignedData(SenderSignedTransaction {
            intent_message: IntentMessage {
                intent: Intent::from(v.intent),
                value: TransactionData::from(&v.data),
            },
            tx_signatures: v.tx_signatures.iter().map(GenericSignature::from).collect(),
        })
    }
}

impl From<&view::SenderSignedData<'_>> for Transaction {
    fn from(v: &view::SenderSignedData<'_>) -> Self {
        Transaction {
            data: SenderSignedData::from(v),
            auth_signature: EmptySignInfo {},
        }
    }
}
