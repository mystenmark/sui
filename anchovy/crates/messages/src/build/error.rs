// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's `SuiError`, as validators send it to clients with a
//! rejection. Encode-only: anchovy sends these and never reads them.

use serde::ser::{SerializeStructVariant, SerializeTupleVariant, Serializer};
use serde::{Deserialize, Serialize};

use super::base::{
    CheckpointContentsDigest, ObjectDigest, ObjectId, ObjectRef, SequenceNumber, SuiAddress,
    TransactionDigest,
};
use super::object::Owner;

/// The reference's `SuiError`, a newtype over its boxed kind: BCS encodes
/// it as the kind alone.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SuiError(pub SuiErrorKind);

/// The variants of the reference's `SuiErrorKind` that anchovy sends. Each
/// serializes at its index in the reference's enum; the others carry types
/// anchovy has no mirror for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuiErrorKind {
    UserInputError {
        error: UserInputError,
    },
    InvalidSignature {
        error: String,
    },
    SignerSignatureAbsent {
        expected: String,
        actual: Vec<String>,
    },
    SignerSignatureNumberMismatch {
        expected: usize,
        actual: usize,
    },
    IncorrectSigner {
        error: String,
    },
    InvalidAddress,
    TransactionDeserializationError {
        error: String,
    },
    KeyConversionError(String),
    TransactionExpired,
    Storage(String),
}

impl Serialize for SuiErrorKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        const NAME: &str = "SuiErrorKind";
        // One `String` field named `error`, as several variants have.
        let error = |s: S, index, variant, error: &String| {
            let mut v = s.serialize_struct_variant(NAME, index, variant, 1)?;
            v.serialize_field("error", error)?;
            v.end()
        };
        match self {
            SuiErrorKind::UserInputError { error } => {
                let mut v = s.serialize_struct_variant(NAME, 0, "UserInputError", 1)?;
                v.serialize_field("error", error)?;
                v.end()
            }
            SuiErrorKind::InvalidSignature { error: e } => error(s, 8, "InvalidSignature", e),
            SuiErrorKind::SignerSignatureAbsent { expected, actual } => {
                let mut v = s.serialize_struct_variant(NAME, 9, "SignerSignatureAbsent", 2)?;
                v.serialize_field("expected", expected)?;
                v.serialize_field("actual", actual)?;
                v.end()
            }
            SuiErrorKind::SignerSignatureNumberMismatch { expected, actual } => {
                let mut v =
                    s.serialize_struct_variant(NAME, 10, "SignerSignatureNumberMismatch", 2)?;
                v.serialize_field("expected", expected)?;
                v.serialize_field("actual", actual)?;
                v.end()
            }
            SuiErrorKind::IncorrectSigner { error: e } => error(s, 11, "IncorrectSigner", e),
            SuiErrorKind::InvalidAddress => s.serialize_unit_variant(NAME, 22, "InvalidAddress"),
            SuiErrorKind::TransactionDeserializationError { error: e } => {
                error(s, 60, "TransactionDeserializationError", e)
            }
            SuiErrorKind::KeyConversionError(e) => {
                s.serialize_newtype_variant(NAME, 74, "KeyConversionError", e)
            }
            SuiErrorKind::TransactionExpired => {
                s.serialize_unit_variant(NAME, 80, "TransactionExpired")
            }
            SuiErrorKind::Storage(e) => {
                let mut v = s.serialize_tuple_variant(NAME, 97, "Storage", 1)?;
                v.serialize_field(e)?;
                v.end()
            }
        }
    }
}

impl From<UserInputError> for SuiErrorKind {
    fn from(error: UserInputError) -> Self {
        SuiErrorKind::UserInputError { error }
    }
}

/// The reference's `UserInputError`, every variant in its order.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum UserInputError {
    MutableObjectUsedMoreThanOnce {
        object_id: ObjectId,
    },
    ObjectInputArityViolation,
    ObjectNotFound {
        object_id: ObjectId,
        version: Option<SequenceNumber>,
    },
    ObjectVersionUnavailableForConsumption {
        provided_obj_ref: ObjectRef,
        current_version: SequenceNumber,
    },
    PackageVerificationTimeout {
        err: String,
    },
    DependentPackageNotFound {
        package_id: ObjectId,
    },
    ImmutableParameterExpectedError {
        object_id: ObjectId,
    },
    SizeLimitExceeded {
        limit: String,
        value: String,
    },
    InvalidChildObjectArgument {
        child_id: ObjectId,
        parent_id: ObjectId,
    },
    InvalidObjectDigest {
        object_id: ObjectId,
        expected_digest: ObjectDigest,
    },
    InvalidSequenceNumber,
    MovePackageAsObject {
        object_id: ObjectId,
    },
    MoveObjectAsPackage {
        object_id: ObjectId,
    },
    IncorrectUserSignature {
        error: String,
    },
    NotSharedObjectError,
    DuplicateObjectRefInput,
    MissingGasPayment,
    GasObjectNotOwnedObject {
        owner: Owner,
    },
    GasBudgetTooHigh {
        gas_budget: u64,
        max_budget: u64,
    },
    GasBudgetTooLow {
        gas_budget: u64,
        min_budget: u64,
    },
    GasBalanceTooLow {
        gas_balance: u128,
        needed_gas_amount: u128,
    },
    UnsupportedSponsoredTransactionKind,
    GasPriceUnderRGP {
        gas_price: u64,
        reference_gas_price: u64,
    },
    GasPriceTooHigh {
        max_gas_price: u64,
    },
    InvalidGasObject {
        object_id: ObjectId,
    },
    InsufficientBalanceToCoverMinimalGas,
    ObjectSequenceNumberTooHigh {
        object_id: ObjectId,
        asked_version: SequenceNumber,
        latest_version: SequenceNumber,
    },
    ObjectDeleted {
        object_ref: ObjectRef,
    },
    InvalidBatchTransaction {
        error: String,
    },
    BlockedMoveFunction,
    EmptyInputCoins,
    UnexpectedGasPaymentObject,
    SharedObjectStartingVersionMismatch,
    TransferObjectWithoutPublicTransferError {
        object_id: ObjectId,
    },
    EmptyCommandInput,
    TransactionDenied {
        error: String,
    },
    Unsupported(String),
    MoveFunctionInputError(String),
    VerifiedCheckpointNotFound(u64),
    VerifiedCheckpointDigestNotFound(String),
    LatestCheckpointSequenceNumberNotFound,
    CheckpointContentsNotFound(CheckpointContentsDigest),
    GenesisTransactionNotFound,
    TransactionCursorNotFound(u64),
    InaccessibleSystemObject {
        object_id: ObjectId,
    },
    MaxPublishCountExceeded {
        max_publish_commands: u64,
        publish_count: u64,
    },
    MutableParameterExpected {
        object_id: ObjectId,
    },
    AddressDeniedForCoin {
        address: SuiAddress,
        coin_type: String,
    },
    PostRandomCommandRestrictions,
    InvalidArgumentIndex {
        command_idx: usize,
        argument_idx: usize,
        index: u16,
    },
    TooManyTransactionsInBatch {
        size: usize,
        limit: u64,
    },
    TotalTransactionSizeTooLargeInBatch {
        size: usize,
        limit: u64,
    },
    NoSharedObjectError {
        digest: TransactionDigest,
    },
    AlreadyExecutedInSoftBundleError {
        digest: TransactionDigest,
    },
    CertificateAlreadyProcessed,
    TransactionAlreadyExecuted {
        digest: TransactionDigest,
    },
    GasPriceMismatchError {
        digest: TransactionDigest,
        expected: u64,
        actual: u64,
    },
    CoinTypeGlobalPause {
        coin_type: String,
    },
    InvalidIdentifier {
        error: String,
    },
    NotOwnedObjectError,
    InvalidWithdrawReservation {
        error: String,
    },
    MissingTransactionExpiration,
    InvalidExpiration {
        error: String,
    },
    InvalidChainId {
        provided: String,
        expected: String,
    },
    RepeatedTransactions {
        digest: TransactionDigest,
    },
    ProposerNotAllowed {
        proposer: u32,
    },
}
