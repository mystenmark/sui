// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{AccountAddress, ObjectId, SuiAddress};
use crate::execution_status as view;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    Success,
    Failure(ExecutionFailure),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExecutionFailure {
    pub error: ExecutionErrorKind,
    pub command: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ModuleId {
    pub address: AccountAddress,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MoveLocation {
    pub module: ModuleId,
    pub function: u16,
    pub instruction: u16,
    pub function_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MoveLocationOpt(pub Option<MoveLocation>);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CongestedObjects(pub Vec<ObjectId>);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ExecutionErrorKind {
    InsufficientGas,
    InvalidGasObject,
    InvariantViolation,
    FeatureNotYetSupported,
    MoveObjectTooBig {
        object_size: u64,
        max_object_size: u64,
    },
    MovePackageTooBig {
        object_size: u64,
        max_object_size: u64,
    },
    CircularObjectOwnership {
        object: ObjectId,
    },
    InsufficientCoinBalance,
    CoinBalanceOverflow,
    PublishErrorNonZeroAddress,
    SuiMoveVerificationError,
    MovePrimitiveRuntimeError(MoveLocationOpt),
    MoveAbort(MoveLocation, u64),
    VMVerificationOrDeserializationError,
    VMInvariantViolation,
    FunctionNotFound,
    ArityMismatch,
    TypeArityMismatch,
    NonEntryFunctionInvoked,
    CommandArgumentError {
        arg_idx: u16,
        kind: CommandArgumentError,
    },
    TypeArgumentError {
        argument_idx: u16,
        kind: TypeArgumentError,
    },
    UnusedValueWithoutDrop {
        result_idx: u16,
        secondary_idx: u16,
    },
    InvalidPublicFunctionReturnType {
        idx: u16,
    },
    InvalidTransferObject,
    EffectsTooLarge {
        current_size: u64,
        max_size: u64,
    },
    PublishUpgradeMissingDependency,
    PublishUpgradeDependencyDowngrade,
    PackageUpgradeError {
        upgrade_error: PackageUpgradeError,
    },
    WrittenObjectsTooLarge {
        current_size: u64,
        max_size: u64,
    },
    CertificateDenied,
    SuiMoveVerificationTimedout,
    SharedObjectOperationNotAllowed,
    InputObjectDeleted,
    ExecutionCancelledDueToSharedObjectCongestion {
        congested_objects: CongestedObjects,
    },
    AddressDeniedForCoin {
        address: SuiAddress,
        coin_type: String,
    },
    CoinTypeGlobalPause {
        coin_type: String,
    },
    ExecutionCancelledDueToRandomnessUnavailable,
    MoveVectorElemTooBig {
        value_size: u64,
        max_scaled_size: u64,
    },
    MoveRawValueTooBig {
        value_size: u64,
        max_scaled_size: u64,
    },
    InvalidLinkage,
    InsufficientFundsForWithdraw,
    NonExclusiveWriteInputObjectModified {
        id: ObjectId,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandArgumentError {
    TypeMismatch,
    InvalidBCSBytes,
    InvalidUsageOfPureArg,
    InvalidArgumentToPrivateEntryFunction,
    IndexOutOfBounds { idx: u16 },
    SecondaryIndexOutOfBounds { result_idx: u16, secondary_idx: u16 },
    InvalidResultArity { result_idx: u16 },
    InvalidGasCoinUsage,
    InvalidValueUsage,
    InvalidObjectByValue,
    InvalidObjectByMutRef,
    SharedObjectOperationNotAllowed,
    InvalidArgumentArity,
    InvalidTransferObject,
    InvalidMakeMoveVecNonObjectArgument,
    ArgumentWithoutValue,
    CannotMoveBorrowedValue,
    CannotWriteToExtendedReference,
    InvalidReferenceArgument,
    InvalidTxContext,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeArgumentError {
    TypeNotFound,
    ConstraintNotSatisfied,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PackageUpgradeError {
    UnableToFetchPackage {
        package_id: ObjectId,
    },
    NotAPackage {
        object_id: ObjectId,
    },
    IncompatibleUpgrade,
    DigestDoesNotMatch {
        digest: Vec<u8>,
    },
    UnknownUpgradePolicy {
        policy: u8,
    },
    PackageIDDoesNotMatch {
        package_id: ObjectId,
        ticket_id: ObjectId,
    },
}

impl From<&view::ExecutionStatus<'_>> for ExecutionStatus {
    fn from(v: &view::ExecutionStatus<'_>) -> Self {
        match v {
            view::ExecutionStatus::Success => ExecutionStatus::Success,
            view::ExecutionStatus::Failure { error, command } => {
                ExecutionStatus::Failure(ExecutionFailure {
                    error: ExecutionErrorKind::from(error),
                    command: *command,
                })
            }
        }
    }
}

impl From<&view::ModuleId<'_>> for ModuleId {
    fn from(v: &view::ModuleId<'_>) -> Self {
        ModuleId {
            address: AccountAddress::from(v.address),
            name: v.name.to_owned(),
        }
    }
}

impl From<&view::MoveLocation<'_>> for MoveLocation {
    fn from(v: &view::MoveLocation<'_>) -> Self {
        MoveLocation {
            module: ModuleId::from(&v.module),
            function: v.function,
            instruction: v.instruction,
            function_name: v.function_name.map(str::to_owned),
        }
    }
}

impl From<&view::ExecutionErrorKind<'_>> for ExecutionErrorKind {
    #[allow(clippy::too_many_lines)]
    fn from(v: &view::ExecutionErrorKind<'_>) -> Self {
        use ExecutionErrorKind as B;
        use view::ExecutionErrorKind as V;
        match *v {
            V::InsufficientGas => B::InsufficientGas,
            V::InvalidGasObject => B::InvalidGasObject,
            V::InvariantViolation => B::InvariantViolation,
            V::FeatureNotYetSupported => B::FeatureNotYetSupported,
            V::MoveObjectTooBig {
                object_size,
                max_object_size,
            } => B::MoveObjectTooBig {
                object_size,
                max_object_size,
            },
            V::MovePackageTooBig {
                object_size,
                max_object_size,
            } => B::MovePackageTooBig {
                object_size,
                max_object_size,
            },
            V::CircularObjectOwnership { object } => B::CircularObjectOwnership {
                object: ObjectId::from(object),
            },
            V::InsufficientCoinBalance => B::InsufficientCoinBalance,
            V::CoinBalanceOverflow => B::CoinBalanceOverflow,
            V::PublishErrorNonZeroAddress => B::PublishErrorNonZeroAddress,
            V::SuiMoveVerificationError => B::SuiMoveVerificationError,
            V::MovePrimitiveRuntimeError(ref location) => B::MovePrimitiveRuntimeError(
                MoveLocationOpt(location.as_ref().map(MoveLocation::from)),
            ),
            V::MoveAbort(ref location, code) => B::MoveAbort(MoveLocation::from(location), code),
            V::VMVerificationOrDeserializationError => B::VMVerificationOrDeserializationError,
            V::VMInvariantViolation => B::VMInvariantViolation,
            V::FunctionNotFound => B::FunctionNotFound,
            V::ArityMismatch => B::ArityMismatch,
            V::TypeArityMismatch => B::TypeArityMismatch,
            V::NonEntryFunctionInvoked => B::NonEntryFunctionInvoked,
            V::CommandArgumentError { arg_idx, ref kind } => B::CommandArgumentError {
                arg_idx,
                kind: CommandArgumentError::from(kind),
            },
            V::TypeArgumentError {
                argument_idx,
                ref kind,
            } => B::TypeArgumentError {
                argument_idx,
                kind: TypeArgumentError::from(kind),
            },
            V::UnusedValueWithoutDrop {
                result_idx,
                secondary_idx,
            } => B::UnusedValueWithoutDrop {
                result_idx,
                secondary_idx,
            },
            V::InvalidPublicFunctionReturnType { idx } => {
                B::InvalidPublicFunctionReturnType { idx }
            }
            V::InvalidTransferObject => B::InvalidTransferObject,
            V::EffectsTooLarge {
                current_size,
                max_size,
            } => B::EffectsTooLarge {
                current_size,
                max_size,
            },
            V::PublishUpgradeMissingDependency => B::PublishUpgradeMissingDependency,
            V::PublishUpgradeDependencyDowngrade => B::PublishUpgradeDependencyDowngrade,
            V::PackageUpgradeError { ref upgrade_error } => B::PackageUpgradeError {
                upgrade_error: PackageUpgradeError::from(upgrade_error),
            },
            V::WrittenObjectsTooLarge {
                current_size,
                max_size,
            } => B::WrittenObjectsTooLarge {
                current_size,
                max_size,
            },
            V::CertificateDenied => B::CertificateDenied,
            V::SuiMoveVerificationTimedout => B::SuiMoveVerificationTimedout,
            V::SharedObjectOperationNotAllowed => B::SharedObjectOperationNotAllowed,
            V::InputObjectDeleted => B::InputObjectDeleted,
            V::ExecutionCancelledDueToSharedObjectCongestion { congested_objects } => {
                B::ExecutionCancelledDueToSharedObjectCongestion {
                    congested_objects: CongestedObjects(
                        congested_objects.iter().map(ObjectId::from).collect(),
                    ),
                }
            }
            V::AddressDeniedForCoin { address, coin_type } => B::AddressDeniedForCoin {
                address: SuiAddress::from(address),
                coin_type: coin_type.to_owned(),
            },
            V::CoinTypeGlobalPause { coin_type } => B::CoinTypeGlobalPause {
                coin_type: coin_type.to_owned(),
            },
            V::ExecutionCancelledDueToRandomnessUnavailable => {
                B::ExecutionCancelledDueToRandomnessUnavailable
            }
            V::MoveVectorElemTooBig {
                value_size,
                max_scaled_size,
            } => B::MoveVectorElemTooBig {
                value_size,
                max_scaled_size,
            },
            V::MoveRawValueTooBig {
                value_size,
                max_scaled_size,
            } => B::MoveRawValueTooBig {
                value_size,
                max_scaled_size,
            },
            V::InvalidLinkage => B::InvalidLinkage,
            V::InsufficientFundsForWithdraw => B::InsufficientFundsForWithdraw,
            V::NonExclusiveWriteInputObjectModified { id } => {
                B::NonExclusiveWriteInputObjectModified {
                    id: ObjectId::from(id),
                }
            }
        }
    }
}

impl From<&view::CommandArgumentError> for CommandArgumentError {
    fn from(v: &view::CommandArgumentError) -> Self {
        use CommandArgumentError as B;
        use view::CommandArgumentError as V;
        match *v {
            V::TypeMismatch => B::TypeMismatch,
            V::InvalidBCSBytes => B::InvalidBCSBytes,
            V::InvalidUsageOfPureArg => B::InvalidUsageOfPureArg,
            V::InvalidArgumentToPrivateEntryFunction => B::InvalidArgumentToPrivateEntryFunction,
            V::IndexOutOfBounds { idx } => B::IndexOutOfBounds { idx },
            V::SecondaryIndexOutOfBounds {
                result_idx,
                secondary_idx,
            } => B::SecondaryIndexOutOfBounds {
                result_idx,
                secondary_idx,
            },
            V::InvalidResultArity { result_idx } => B::InvalidResultArity { result_idx },
            V::InvalidGasCoinUsage => B::InvalidGasCoinUsage,
            V::InvalidValueUsage => B::InvalidValueUsage,
            V::InvalidObjectByValue => B::InvalidObjectByValue,
            V::InvalidObjectByMutRef => B::InvalidObjectByMutRef,
            V::SharedObjectOperationNotAllowed => B::SharedObjectOperationNotAllowed,
            V::InvalidArgumentArity => B::InvalidArgumentArity,
            V::InvalidTransferObject => B::InvalidTransferObject,
            V::InvalidMakeMoveVecNonObjectArgument => B::InvalidMakeMoveVecNonObjectArgument,
            V::ArgumentWithoutValue => B::ArgumentWithoutValue,
            V::CannotMoveBorrowedValue => B::CannotMoveBorrowedValue,
            V::CannotWriteToExtendedReference => B::CannotWriteToExtendedReference,
            V::InvalidReferenceArgument => B::InvalidReferenceArgument,
            V::InvalidTxContext => B::InvalidTxContext,
        }
    }
}

impl From<&view::TypeArgumentError> for TypeArgumentError {
    fn from(v: &view::TypeArgumentError) -> Self {
        match v {
            view::TypeArgumentError::TypeNotFound => TypeArgumentError::TypeNotFound,
            view::TypeArgumentError::ConstraintNotSatisfied => {
                TypeArgumentError::ConstraintNotSatisfied
            }
        }
    }
}

impl From<&view::PackageUpgradeError<'_>> for PackageUpgradeError {
    fn from(v: &view::PackageUpgradeError<'_>) -> Self {
        use PackageUpgradeError as B;
        use view::PackageUpgradeError as V;
        match *v {
            V::UnableToFetchPackage { package_id } => B::UnableToFetchPackage {
                package_id: ObjectId::from(package_id),
            },
            V::NotAPackage { object_id } => B::NotAPackage {
                object_id: ObjectId::from(object_id),
            },
            V::IncompatibleUpgrade => B::IncompatibleUpgrade,
            V::DigestDoesNotMatch { digest } => B::DigestDoesNotMatch {
                digest: digest.to_vec(),
            },
            V::UnknownUpgradePolicy { policy } => B::UnknownUpgradePolicy { policy },
            V::PackageIDDoesNotMatch {
                package_id,
                ticket_id,
            } => B::PackageIDDoesNotMatch {
                package_id: ObjectId::from(package_id),
                ticket_id: ObjectId::from(ticket_id),
            },
        }
    }
}
