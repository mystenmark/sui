// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

pub use messages::build::error::{SuiErrorKind, UserInputError};

/// The reference's error variant: `SuiErrorKind`'s, or `UserInputError`'s
/// when the reference wraps one. Tests compare these names with the
/// reference's verdicts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    TransactionExpired,
    Unsupported,
    InvalidExpiration,
    InvalidChainId,
    MissingGasPayment,
    InvalidWithdrawReservation,
    GasPriceUnderRGP,
    SizeLimitExceeded,
    GasObjectNotOwnedObject,
    GasPriceTooHigh,
    GasBudgetTooHigh,
    GasBudgetTooLow,
    UnsupportedSponsoredTransactionKind,
    DuplicateObjectRefInput,
    MaxPublishCountExceeded,
    EmptyCommandInput,
    InvalidIdentifier,
    InvalidArgumentIndex,
    PostRandomCommandRestrictions,
    /// The reference fails the whole request while deserializing: our
    /// parser defers these checks to validation.
    TransactionDeserializationError,
    SignerSignatureNumberMismatch,
    SignerSignatureAbsent,
    InvalidSignature,
    IncorrectSigner,
    InvalidAddress,
    KeyConversionError,
    // The stateful input checks (`inputs`).
    ObjectNotFound,
    DependentPackageNotFound,
    MoveObjectAsPackage,
    MovePackageAsObject,
    ObjectVersionUnavailableForConsumption,
    InvalidGasObject,
    GasBalanceTooLow,
    MutableObjectUsedMoreThanOnce,
    ObjectInputArityViolation,
    InvalidSequenceNumber,
    InvalidObjectDigest,
    InvalidChildObjectArgument,
    NotOwnedObjectError,
    NotSharedObjectError,
    ImmutableParameterExpectedError,
    SharedObjectStartingVersionMismatch,
    IncorrectUserSignature,
    MutableParameterExpected,
}

impl ErrorKind {
    /// Every kind, for coverage checks.
    pub const ALL: &[ErrorKind] = &[
        ErrorKind::TransactionExpired,
        ErrorKind::Unsupported,
        ErrorKind::InvalidExpiration,
        ErrorKind::InvalidChainId,
        ErrorKind::MissingGasPayment,
        ErrorKind::InvalidWithdrawReservation,
        ErrorKind::GasPriceUnderRGP,
        ErrorKind::SizeLimitExceeded,
        ErrorKind::GasObjectNotOwnedObject,
        ErrorKind::GasPriceTooHigh,
        ErrorKind::GasBudgetTooHigh,
        ErrorKind::GasBudgetTooLow,
        ErrorKind::UnsupportedSponsoredTransactionKind,
        ErrorKind::DuplicateObjectRefInput,
        ErrorKind::MaxPublishCountExceeded,
        ErrorKind::EmptyCommandInput,
        ErrorKind::InvalidIdentifier,
        ErrorKind::InvalidArgumentIndex,
        ErrorKind::PostRandomCommandRestrictions,
        ErrorKind::TransactionDeserializationError,
        ErrorKind::SignerSignatureNumberMismatch,
        ErrorKind::SignerSignatureAbsent,
        ErrorKind::InvalidSignature,
        ErrorKind::IncorrectSigner,
        ErrorKind::InvalidAddress,
        ErrorKind::KeyConversionError,
        ErrorKind::ObjectNotFound,
        ErrorKind::DependentPackageNotFound,
        ErrorKind::MoveObjectAsPackage,
        ErrorKind::MovePackageAsObject,
        ErrorKind::ObjectVersionUnavailableForConsumption,
        ErrorKind::InvalidGasObject,
        ErrorKind::GasBalanceTooLow,
        ErrorKind::MutableObjectUsedMoreThanOnce,
        ErrorKind::ObjectInputArityViolation,
        ErrorKind::InvalidSequenceNumber,
        ErrorKind::InvalidObjectDigest,
        ErrorKind::InvalidChildObjectArgument,
        ErrorKind::NotOwnedObjectError,
        ErrorKind::NotSharedObjectError,
        ErrorKind::ImmutableParameterExpectedError,
        ErrorKind::SharedObjectStartingVersionMismatch,
        ErrorKind::IncorrectUserSignature,
        ErrorKind::MutableParameterExpected,
    ];

    // Adding a kind breaks this match until `ALL` lists it too.
    #[allow(dead_code)]
    fn listed_in_all(self) {
        match self {
            ErrorKind::TransactionExpired
            | ErrorKind::Unsupported
            | ErrorKind::InvalidExpiration
            | ErrorKind::InvalidChainId
            | ErrorKind::MissingGasPayment
            | ErrorKind::InvalidWithdrawReservation
            | ErrorKind::GasPriceUnderRGP
            | ErrorKind::SizeLimitExceeded
            | ErrorKind::GasObjectNotOwnedObject
            | ErrorKind::GasPriceTooHigh
            | ErrorKind::GasBudgetTooHigh
            | ErrorKind::GasBudgetTooLow
            | ErrorKind::UnsupportedSponsoredTransactionKind
            | ErrorKind::DuplicateObjectRefInput
            | ErrorKind::MaxPublishCountExceeded
            | ErrorKind::EmptyCommandInput
            | ErrorKind::InvalidIdentifier
            | ErrorKind::InvalidArgumentIndex
            | ErrorKind::PostRandomCommandRestrictions
            | ErrorKind::TransactionDeserializationError
            | ErrorKind::SignerSignatureNumberMismatch
            | ErrorKind::SignerSignatureAbsent
            | ErrorKind::InvalidSignature
            | ErrorKind::IncorrectSigner
            | ErrorKind::InvalidAddress
            | ErrorKind::KeyConversionError
            | ErrorKind::ObjectNotFound
            | ErrorKind::DependentPackageNotFound
            | ErrorKind::MoveObjectAsPackage
            | ErrorKind::MovePackageAsObject
            | ErrorKind::ObjectVersionUnavailableForConsumption
            | ErrorKind::InvalidGasObject
            | ErrorKind::GasBalanceTooLow
            | ErrorKind::MutableObjectUsedMoreThanOnce
            | ErrorKind::ObjectInputArityViolation
            | ErrorKind::InvalidSequenceNumber
            | ErrorKind::InvalidObjectDigest
            | ErrorKind::InvalidChildObjectArgument
            | ErrorKind::NotOwnedObjectError
            | ErrorKind::NotSharedObjectError
            | ErrorKind::ImmutableParameterExpectedError
            | ErrorKind::SharedObjectStartingVersionMismatch
            | ErrorKind::IncorrectUserSignature
            | ErrorKind::MutableParameterExpected => {}
        }
    }
}

/// A rejection: the reference's error, as it sends it to clients.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub reason: SuiErrorKind,
}

impl Error {
    pub fn new(reason: impl Into<SuiErrorKind>) -> Error {
        Error {
            reason: reason.into(),
        }
    }

    /// The reason's variant.
    ///
    /// # Panics
    ///
    /// For a reason validation never gives.
    pub fn kind(&self) -> ErrorKind {
        match &self.reason {
            SuiErrorKind::TransactionExpired => ErrorKind::TransactionExpired,
            SuiErrorKind::InvalidSignature { .. } => ErrorKind::InvalidSignature,
            SuiErrorKind::SignerSignatureAbsent { .. } => ErrorKind::SignerSignatureAbsent,
            SuiErrorKind::SignerSignatureNumberMismatch { .. } => {
                ErrorKind::SignerSignatureNumberMismatch
            }
            SuiErrorKind::IncorrectSigner { .. } => ErrorKind::IncorrectSigner,
            SuiErrorKind::InvalidAddress => ErrorKind::InvalidAddress,
            SuiErrorKind::TransactionDeserializationError { .. } => {
                ErrorKind::TransactionDeserializationError
            }
            SuiErrorKind::KeyConversionError(_) => ErrorKind::KeyConversionError,
            SuiErrorKind::UserInputError { error } => match error {
                UserInputError::Unsupported(_) => ErrorKind::Unsupported,
                UserInputError::InvalidExpiration { .. } => ErrorKind::InvalidExpiration,
                UserInputError::InvalidChainId { .. } => ErrorKind::InvalidChainId,
                UserInputError::MissingGasPayment => ErrorKind::MissingGasPayment,
                UserInputError::InvalidWithdrawReservation { .. } => {
                    ErrorKind::InvalidWithdrawReservation
                }
                UserInputError::GasPriceUnderRGP { .. } => ErrorKind::GasPriceUnderRGP,
                UserInputError::SizeLimitExceeded { .. } => ErrorKind::SizeLimitExceeded,
                UserInputError::GasObjectNotOwnedObject { .. } => {
                    ErrorKind::GasObjectNotOwnedObject
                }
                UserInputError::GasPriceTooHigh { .. } => ErrorKind::GasPriceTooHigh,
                UserInputError::GasBudgetTooHigh { .. } => ErrorKind::GasBudgetTooHigh,
                UserInputError::GasBudgetTooLow { .. } => ErrorKind::GasBudgetTooLow,
                UserInputError::UnsupportedSponsoredTransactionKind => {
                    ErrorKind::UnsupportedSponsoredTransactionKind
                }
                UserInputError::DuplicateObjectRefInput => ErrorKind::DuplicateObjectRefInput,
                UserInputError::MaxPublishCountExceeded { .. } => {
                    ErrorKind::MaxPublishCountExceeded
                }
                UserInputError::EmptyCommandInput => ErrorKind::EmptyCommandInput,
                UserInputError::InvalidIdentifier { .. } => ErrorKind::InvalidIdentifier,
                UserInputError::InvalidArgumentIndex { .. } => ErrorKind::InvalidArgumentIndex,
                UserInputError::PostRandomCommandRestrictions => {
                    ErrorKind::PostRandomCommandRestrictions
                }
                UserInputError::ObjectNotFound { .. } => ErrorKind::ObjectNotFound,
                UserInputError::DependentPackageNotFound { .. } => {
                    ErrorKind::DependentPackageNotFound
                }
                UserInputError::MoveObjectAsPackage { .. } => ErrorKind::MoveObjectAsPackage,
                UserInputError::MovePackageAsObject { .. } => ErrorKind::MovePackageAsObject,
                UserInputError::ObjectVersionUnavailableForConsumption { .. } => {
                    ErrorKind::ObjectVersionUnavailableForConsumption
                }
                UserInputError::InvalidGasObject { .. } => ErrorKind::InvalidGasObject,
                UserInputError::GasBalanceTooLow { .. } => ErrorKind::GasBalanceTooLow,
                UserInputError::MutableObjectUsedMoreThanOnce { .. } => {
                    ErrorKind::MutableObjectUsedMoreThanOnce
                }
                UserInputError::ObjectInputArityViolation => ErrorKind::ObjectInputArityViolation,
                UserInputError::InvalidSequenceNumber => ErrorKind::InvalidSequenceNumber,
                UserInputError::InvalidObjectDigest { .. } => ErrorKind::InvalidObjectDigest,
                UserInputError::InvalidChildObjectArgument { .. } => {
                    ErrorKind::InvalidChildObjectArgument
                }
                UserInputError::NotOwnedObjectError => ErrorKind::NotOwnedObjectError,
                UserInputError::NotSharedObjectError => ErrorKind::NotSharedObjectError,
                UserInputError::ImmutableParameterExpectedError { .. } => {
                    ErrorKind::ImmutableParameterExpectedError
                }
                UserInputError::SharedObjectStartingVersionMismatch => {
                    ErrorKind::SharedObjectStartingVersionMismatch
                }
                UserInputError::IncorrectUserSignature { .. } => ErrorKind::IncorrectUserSignature,
                UserInputError::MutableParameterExpected { .. } => {
                    ErrorKind::MutableParameterExpected
                }
                error => unreachable!("validation does not reject with {error:?}"),
            },
            reason => unreachable!("validation does not reject with {reason:?}"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.reason)
    }
}

impl std::error::Error for Error {}
