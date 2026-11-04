// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Anchovy's mirror of the reference's `SuiError` encodes as the reference
//! does: each variant decodes as the reference's variant of the same name,
//! which encodes back to the same bytes.

use messages::build::base::{
    AccountAddress, CheckpointContentsDigest, Digest, ObjectDigest, ObjectId, SequenceNumber,
    SuiAddress, TransactionDigest,
};
use messages::build::error::{SuiError, SuiErrorKind, UserInputError as U};
use messages::build::object::Owner;

fn name(debug: String) -> String {
    debug.split([' ', '(', '{']).next().unwrap().to_owned()
}

fn check(kind: SuiErrorKind) {
    let bytes = bcs::to_bytes(&SuiError(kind.clone())).unwrap();
    let reference: sui_types::error::SuiError = bcs::from_bytes(&bytes)
        .unwrap_or_else(|e| panic!("{kind:?} does not decode as the reference's: {e}"));
    assert_eq!(bcs::to_bytes(&reference).unwrap(), bytes, "{kind:?}");
    let (mirror, reference) = match (&kind, reference.as_inner()) {
        (
            SuiErrorKind::UserInputError { error },
            sui_types::error::SuiErrorKind::UserInputError { error: reference },
        ) => (
            name(format!("{error:?}")),
            <&'static str>::from(reference).to_owned(),
        ),
        (_, reference) => (
            name(format!("{kind:?}")),
            <&'static str>::from(reference).to_owned(),
        ),
    };
    assert_eq!(mirror, reference);
}

#[test]
fn every_variant_encodes_as_the_reference_s() {
    let id = ObjectId(AccountAddress([7; 32]));
    let other = ObjectId(AccountAddress([9; 32]));
    let version = SequenceNumber(5);
    let digest = Digest([3; 32]);
    let object_ref = (id, version, ObjectDigest(digest));
    let tx = TransactionDigest(digest);
    let s = || "s".to_owned();
    let user = [
        U::MutableObjectUsedMoreThanOnce { object_id: id },
        U::ObjectInputArityViolation,
        U::ObjectNotFound {
            object_id: id,
            version: Some(version),
        },
        U::ObjectVersionUnavailableForConsumption {
            provided_obj_ref: object_ref,
            current_version: SequenceNumber(6),
        },
        U::PackageVerificationTimeout { err: s() },
        U::DependentPackageNotFound { package_id: id },
        U::ImmutableParameterExpectedError { object_id: id },
        U::SizeLimitExceeded {
            limit: s(),
            value: s(),
        },
        U::InvalidChildObjectArgument {
            child_id: id,
            parent_id: other,
        },
        U::InvalidObjectDigest {
            object_id: id,
            expected_digest: ObjectDigest(digest),
        },
        U::InvalidSequenceNumber,
        U::MovePackageAsObject { object_id: id },
        U::MoveObjectAsPackage { object_id: id },
        U::IncorrectUserSignature { error: s() },
        U::NotSharedObjectError,
        U::DuplicateObjectRefInput,
        U::MissingGasPayment,
        U::GasObjectNotOwnedObject {
            owner: Owner::Shared {
                initial_shared_version: version,
            },
        },
        U::GasBudgetTooHigh {
            gas_budget: 1,
            max_budget: 2,
        },
        U::GasBudgetTooLow {
            gas_budget: 1,
            min_budget: 2,
        },
        U::GasBalanceTooLow {
            gas_balance: 1,
            needed_gas_amount: 2,
        },
        U::UnsupportedSponsoredTransactionKind,
        U::GasPriceUnderRGP {
            gas_price: 1,
            reference_gas_price: 2,
        },
        U::GasPriceTooHigh { max_gas_price: 1 },
        U::InvalidGasObject { object_id: id },
        U::InsufficientBalanceToCoverMinimalGas,
        U::ObjectSequenceNumberTooHigh {
            object_id: id,
            asked_version: SequenceNumber(6),
            latest_version: version,
        },
        U::ObjectDeleted { object_ref },
        U::InvalidBatchTransaction { error: s() },
        U::BlockedMoveFunction,
        U::EmptyInputCoins,
        U::UnexpectedGasPaymentObject,
        U::SharedObjectStartingVersionMismatch,
        U::TransferObjectWithoutPublicTransferError { object_id: id },
        U::EmptyCommandInput,
        U::TransactionDenied { error: s() },
        U::Unsupported(s()),
        U::MoveFunctionInputError(s()),
        U::VerifiedCheckpointNotFound(4),
        U::VerifiedCheckpointDigestNotFound(s()),
        U::LatestCheckpointSequenceNumberNotFound,
        U::CheckpointContentsNotFound(CheckpointContentsDigest(digest)),
        U::GenesisTransactionNotFound,
        U::TransactionCursorNotFound(4),
        U::InaccessibleSystemObject { object_id: id },
        U::MaxPublishCountExceeded {
            max_publish_commands: 1,
            publish_count: 2,
        },
        U::MutableParameterExpected { object_id: id },
        U::AddressDeniedForCoin {
            address: SuiAddress([8; 32]),
            coin_type: s(),
        },
        U::PostRandomCommandRestrictions,
        U::InvalidArgumentIndex {
            command_idx: 1,
            argument_idx: 2,
            index: 3,
        },
        U::TooManyTransactionsInBatch { size: 1, limit: 2 },
        U::TotalTransactionSizeTooLargeInBatch { size: 1, limit: 2 },
        U::NoSharedObjectError { digest: tx },
        U::AlreadyExecutedInSoftBundleError { digest: tx },
        U::CertificateAlreadyProcessed,
        U::TransactionAlreadyExecuted { digest: tx },
        U::GasPriceMismatchError {
            digest: tx,
            expected: 1,
            actual: 2,
        },
        U::CoinTypeGlobalPause { coin_type: s() },
        U::InvalidIdentifier { error: s() },
        U::NotOwnedObjectError,
        U::InvalidWithdrawReservation { error: s() },
        U::MissingTransactionExpiration,
        U::InvalidExpiration { error: s() },
        U::InvalidChainId {
            provided: s(),
            expected: s(),
        },
        U::RepeatedTransactions { digest: tx },
        U::ProposerNotAllowed { proposer: 3 },
    ];
    for error in user {
        check(error.into());
    }
    for kind in [
        SuiErrorKind::InvalidSignature { error: s() },
        SuiErrorKind::SignerSignatureAbsent {
            expected: s(),
            actual: vec![s(), s()],
        },
        SuiErrorKind::SignerSignatureNumberMismatch {
            expected: 1,
            actual: 2,
        },
        SuiErrorKind::IncorrectSigner { error: s() },
        SuiErrorKind::InvalidAddress,
        SuiErrorKind::TransactionDeserializationError { error: s() },
        SuiErrorKind::KeyConversionError(s()),
        SuiErrorKind::TransactionExpired,
        SuiErrorKind::Storage(s()),
    ] {
        check(kind);
    }
}
