// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::base::{AccountAddress, ObjectId, SuiAddress};
use crate::error::{ParseError, Result};
use crate::reader::Reader;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecutionStatus<'a> {
    Success,
    Failure {
        error: ExecutionErrorKind<'a>,
        command: Option<u64>,
    },
}

impl<'a> ExecutionStatus<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<ExecutionStatus<'a>> {
        match r.variant()? {
            0 => Ok(ExecutionStatus::Success),
            1 => Ok(ExecutionStatus::Failure {
                error: ExecutionErrorKind::parse(r)?,
                command: r.option_u64()?,
            }),
            tag => Err(ParseError::UnknownVariant {
                ty: "ExecutionStatus",
                tag,
            }),
        }
    }
}

/// The module name is not checked against the Move identifier grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModuleId<'a> {
    pub address: &'a AccountAddress,
    pub name: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MoveLocation<'a> {
    pub module: ModuleId<'a>,
    pub function: u16,
    pub instruction: u16,
    pub function_name: Option<&'a str>,
}

impl<'a> MoveLocation<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<MoveLocation<'a>> {
        Ok(MoveLocation {
            module: ModuleId {
                address: AccountAddress::parse(r)?,
                name: r.str()?,
            },
            function: r.u16()?,
            instruction: r.u16()?,
            function_name: if r.option()? { Some(r.str()?) } else { None },
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecutionErrorKind<'a> {
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
        object: &'a ObjectId,
    },
    InsufficientCoinBalance,
    CoinBalanceOverflow,
    PublishErrorNonZeroAddress,
    SuiMoveVerificationError,
    MovePrimitiveRuntimeError(Option<MoveLocation<'a>>),
    MoveAbort(MoveLocation<'a>, u64),
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
        upgrade_error: PackageUpgradeError<'a>,
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
        congested_objects: &'a [ObjectId],
    },
    AddressDeniedForCoin {
        address: &'a SuiAddress,
        coin_type: &'a str,
    },
    CoinTypeGlobalPause {
        coin_type: &'a str,
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
        id: &'a ObjectId,
    },
}

impl<'a> ExecutionErrorKind<'a> {
    #[allow(clippy::too_many_lines)]
    pub fn parse(r: &mut Reader<'a>) -> Result<ExecutionErrorKind<'a>> {
        use ExecutionErrorKind as E;
        Ok(match r.variant()? {
            0 => E::InsufficientGas,
            1 => E::InvalidGasObject,
            2 => E::InvariantViolation,
            3 => E::FeatureNotYetSupported,
            4 => E::MoveObjectTooBig {
                object_size: r.u64()?,
                max_object_size: r.u64()?,
            },
            5 => E::MovePackageTooBig {
                object_size: r.u64()?,
                max_object_size: r.u64()?,
            },
            6 => E::CircularObjectOwnership {
                object: ObjectId::parse(r)?,
            },
            7 => E::InsufficientCoinBalance,
            8 => E::CoinBalanceOverflow,
            9 => E::PublishErrorNonZeroAddress,
            10 => E::SuiMoveVerificationError,
            11 => E::MovePrimitiveRuntimeError(if r.option()? {
                Some(MoveLocation::parse(r)?)
            } else {
                None
            }),
            12 => E::MoveAbort(MoveLocation::parse(r)?, r.u64()?),
            13 => E::VMVerificationOrDeserializationError,
            14 => E::VMInvariantViolation,
            15 => E::FunctionNotFound,
            16 => E::ArityMismatch,
            17 => E::TypeArityMismatch,
            18 => E::NonEntryFunctionInvoked,
            19 => E::CommandArgumentError {
                arg_idx: r.u16()?,
                kind: CommandArgumentError::parse(r)?,
            },
            20 => E::TypeArgumentError {
                argument_idx: r.u16()?,
                kind: TypeArgumentError::parse(r)?,
            },
            21 => E::UnusedValueWithoutDrop {
                result_idx: r.u16()?,
                secondary_idx: r.u16()?,
            },
            22 => E::InvalidPublicFunctionReturnType { idx: r.u16()? },
            23 => E::InvalidTransferObject,
            24 => E::EffectsTooLarge {
                current_size: r.u64()?,
                max_size: r.u64()?,
            },
            25 => E::PublishUpgradeMissingDependency,
            26 => E::PublishUpgradeDependencyDowngrade,
            27 => E::PackageUpgradeError {
                upgrade_error: PackageUpgradeError::parse(r)?,
            },
            28 => E::WrittenObjectsTooLarge {
                current_size: r.u64()?,
                max_size: r.u64()?,
            },
            29 => E::CertificateDenied,
            30 => E::SuiMoveVerificationTimedout,
            31 => E::SharedObjectOperationNotAllowed,
            32 => E::InputObjectDeleted,
            33 => E::ExecutionCancelledDueToSharedObjectCongestion {
                congested_objects: r.record_vec()?,
            },
            34 => E::AddressDeniedForCoin {
                address: SuiAddress::parse(r)?,
                coin_type: r.str()?,
            },
            35 => E::CoinTypeGlobalPause {
                coin_type: r.str()?,
            },
            36 => E::ExecutionCancelledDueToRandomnessUnavailable,
            37 => E::MoveVectorElemTooBig {
                value_size: r.u64()?,
                max_scaled_size: r.u64()?,
            },
            38 => E::MoveRawValueTooBig {
                value_size: r.u64()?,
                max_scaled_size: r.u64()?,
            },
            39 => E::InvalidLinkage,
            40 => E::InsufficientFundsForWithdraw,
            41 => E::NonExclusiveWriteInputObjectModified {
                id: ObjectId::parse(r)?,
            },
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ExecutionErrorKind",
                    tag,
                });
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

impl CommandArgumentError {
    pub fn parse(r: &mut Reader<'_>) -> Result<CommandArgumentError> {
        use CommandArgumentError as E;
        Ok(match r.variant()? {
            0 => E::TypeMismatch,
            1 => E::InvalidBCSBytes,
            2 => E::InvalidUsageOfPureArg,
            3 => E::InvalidArgumentToPrivateEntryFunction,
            4 => E::IndexOutOfBounds { idx: r.u16()? },
            5 => E::SecondaryIndexOutOfBounds {
                result_idx: r.u16()?,
                secondary_idx: r.u16()?,
            },
            6 => E::InvalidResultArity {
                result_idx: r.u16()?,
            },
            7 => E::InvalidGasCoinUsage,
            8 => E::InvalidValueUsage,
            9 => E::InvalidObjectByValue,
            10 => E::InvalidObjectByMutRef,
            11 => E::SharedObjectOperationNotAllowed,
            12 => E::InvalidArgumentArity,
            13 => E::InvalidTransferObject,
            14 => E::InvalidMakeMoveVecNonObjectArgument,
            15 => E::ArgumentWithoutValue,
            16 => E::CannotMoveBorrowedValue,
            17 => E::CannotWriteToExtendedReference,
            18 => E::InvalidReferenceArgument,
            19 => E::InvalidTxContext,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "CommandArgumentError",
                    tag,
                });
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeArgumentError {
    TypeNotFound,
    ConstraintNotSatisfied,
}

impl TypeArgumentError {
    pub fn parse(r: &mut Reader<'_>) -> Result<TypeArgumentError> {
        match r.variant()? {
            0 => Ok(TypeArgumentError::TypeNotFound),
            1 => Ok(TypeArgumentError::ConstraintNotSatisfied),
            tag => Err(ParseError::UnknownVariant {
                ty: "TypeArgumentError",
                tag,
            }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackageUpgradeError<'a> {
    UnableToFetchPackage {
        package_id: &'a ObjectId,
    },
    NotAPackage {
        object_id: &'a ObjectId,
    },
    IncompatibleUpgrade,
    DigestDoesNotMatch {
        digest: &'a [u8],
    },
    UnknownUpgradePolicy {
        policy: u8,
    },
    PackageIDDoesNotMatch {
        package_id: &'a ObjectId,
        ticket_id: &'a ObjectId,
    },
}

impl<'a> PackageUpgradeError<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<PackageUpgradeError<'a>> {
        use PackageUpgradeError as E;
        Ok(match r.variant()? {
            0 => E::UnableToFetchPackage {
                package_id: ObjectId::parse(r)?,
            },
            1 => E::NotAPackage {
                object_id: ObjectId::parse(r)?,
            },
            2 => E::IncompatibleUpgrade,
            3 => E::DigestDoesNotMatch {
                digest: r.byte_vec()?,
            },
            4 => E::UnknownUpgradePolicy { policy: r.u8()? },
            5 => E::PackageIDDoesNotMatch {
                package_id: ObjectId::parse(r)?,
                ticket_id: ObjectId::parse(r)?,
            },
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "PackageUpgradeError",
                    tag,
                });
            }
        })
    }
}
