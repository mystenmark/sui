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
