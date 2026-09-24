// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

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
            | ErrorKind::KeyConversionError => {}
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub kind: ErrorKind,
    /// What failed, for people; not compared.
    pub detail: String,
}

impl Error {
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Error {
        Error {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for Error {}
