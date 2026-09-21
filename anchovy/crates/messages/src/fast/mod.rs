// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Builders for the messages a validator produces: effects, events,
//! checkpoint contents and summaries. Each gathers its parts in a
//! [`Bump`], writes the BCS once into that arena, and hashes what it wrote.
//! Build, serialize and drop is one allocation and one free when the arena
//! was sized well.
//!
//! The wire layout each builder writes is the one the parsers in this
//! crate read; `tests/fast.rs` rebuilds every mainnet message from its
//! parsed view and checks bytes and digest.

use blake2::Blake2b;
use blake2::digest::consts::U32;

pub use containers::Bump;

use crate::base::{AccountAddress, Digest, SuiAddress};
use crate::execution_status::{
    CommandArgumentError, ExecutionErrorKind, ExecutionStatus, MoveLocation, PackageUpgradeError,
    TypeArgumentError,
};
use crate::object::Owner;
use crate::type_tag::{StructTag, TypeTag};

pub mod checkpoint;
pub mod effects;
pub mod events;

pub use checkpoint::{ContentsBuilder, SummaryBuilder};
pub use effects::EffectsBuilder;
pub use events::EventsBuilder;

/// A finished message: its BCS bytes in the builder's arena, and its digest
/// if the type has one.
#[derive(Clone, Copy, Debug)]
pub struct Built<'a> {
    pub bytes: &'a [u8],
    pub digest: Digest,
}

/// Writes BCS into an arena, the mirror of `Reader`.
pub struct Writer<'a> {
    out: containers::Vec<'a, u8>,
}

impl<'a> Writer<'a> {
    pub fn new_in(bump: &'a Bump, capacity: usize) -> Writer<'a> {
        Writer {
            out: containers::Vec::with_capacity_in(capacity, bump),
        }
    }

    pub fn len(&self) -> usize {
        self.out.len()
    }

    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    /// The bytes written, and the digest of `type_name`, `::`, and them.
    pub fn finish(self, type_name: &str) -> Built<'a> {
        use blake2::Digest as _;
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(type_name.as_bytes());
        hasher.update(b"::");
        hasher.update(&self.out);
        Built {
            bytes: self.out.leak(),
            digest: Digest::new(hasher.finalize().into()),
        }
    }

    /// The bytes written, for types that have no digest.
    pub fn finish_bytes(self) -> &'a [u8] {
        self.out.leak()
    }

    #[inline]
    pub fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    #[inline]
    pub fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn bool(&mut self, v: bool) {
        self.out.push(u8::from(v));
    }

    #[inline]
    pub fn uleb128(&mut self, mut v: u32) {
        while v >= 0x80 {
            self.out.push((v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }

    /// A sequence length or a variant index.
    #[inline]
    pub fn len_prefix(&mut self, n: usize) {
        self.uleb128(u32::try_from(n).expect("a sequence of at most u32::MAX"));
    }

    /// Bytes without a length, for fixed-size fields.
    #[inline]
    pub fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// A length-prefixed byte string.
    #[inline]
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.len_prefix(bytes.len());
        self.out.extend_from_slice(bytes);
    }

    #[inline]
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    #[inline]
    pub fn option_u64(&mut self, v: Option<u64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.u64(v);
            }
            None => self.u8(0),
        }
    }

    /// A digest: its length byte and its bytes, as it sits on the wire.
    #[inline]
    pub fn digest(&mut self, d: &Digest) {
        self.bytes(&d.bytes);
    }

    #[inline]
    pub fn option_digest(&mut self, d: Option<&Digest>) {
        match d {
            Some(d) => {
                self.u8(1);
                self.digest(d);
            }
            None => self.u8(0),
        }
    }

    pub fn owner(&mut self, owner: &Owner<'_>) {
        match owner {
            Owner::AddressOwner(a) => {
                self.u8(0);
                self.raw(&a.0);
            }
            Owner::ObjectOwner(a) => {
                self.u8(1);
                self.raw(&a.0);
            }
            Owner::Shared {
                initial_shared_version,
            } => {
                self.u8(2);
                self.u64(*initial_shared_version);
            }
            Owner::Immutable => self.u8(3),
            Owner::ConsensusAddressOwner {
                start_version,
                owner,
            } => {
                self.u8(4);
                self.u64(*start_version);
                self.raw(&owner.0);
            }
            Owner::Party(party) => {
                self.u8(5);
                self.u64(party.start_version);
                self.u64(party.default_permissions);
                self.len_prefix(party.members.len());
                for m in party.members {
                    self.raw(&m.address.0);
                    self.raw(&m.permissions.0);
                }
            }
        }
    }

    pub fn type_tag(&mut self, tag: &TypeTag<'_>) {
        match tag {
            TypeTag::Bool => self.u8(0),
            TypeTag::U8 => self.u8(1),
            TypeTag::U64 => self.u8(2),
            TypeTag::U128 => self.u8(3),
            TypeTag::Address => self.u8(4),
            TypeTag::Signer => self.u8(5),
            TypeTag::Vector(inner) => {
                self.u8(6);
                self.type_tag(inner);
            }
            TypeTag::Struct(s) => {
                self.u8(7);
                self.struct_tag(s);
            }
            TypeTag::U16 => self.u8(8),
            TypeTag::U32 => self.u8(9),
            TypeTag::U256 => self.u8(10),
        }
    }

    pub fn struct_tag(&mut self, s: &StructTag<'_>) {
        self.raw(&s.address.0);
        self.str(s.module);
        self.str(s.name);
        self.len_prefix(s.type_params.len());
        for p in s.type_params {
            self.type_tag(p);
        }
    }

    fn move_location(&mut self, l: &MoveLocation<'_>) {
        self.raw(&l.module.address.0);
        self.str(l.module.name);
        self.u16(l.function);
        self.u16(l.instruction);
        match l.function_name {
            Some(name) => {
                self.u8(1);
                self.str(name);
            }
            None => self.u8(0),
        }
    }

    pub fn execution_status(&mut self, status: &ExecutionStatus<'_>) {
        match status {
            ExecutionStatus::Success => self.u8(0),
            ExecutionStatus::Failure { error, command } => {
                self.u8(1);
                self.execution_error(error);
                self.option_u64(*command);
            }
        }
    }

    // One arm per variant of a 42-variant enum.
    #[allow(clippy::too_many_lines)]
    fn execution_error(&mut self, error: &ExecutionErrorKind<'_>) {
        use ExecutionErrorKind as E;
        match error {
            E::InsufficientGas => self.u8(0),
            E::InvalidGasObject => self.u8(1),
            E::InvariantViolation => self.u8(2),
            E::FeatureNotYetSupported => self.u8(3),
            E::MoveObjectTooBig {
                object_size,
                max_object_size,
            } => {
                self.u8(4);
                self.u64(*object_size);
                self.u64(*max_object_size);
            }
            E::MovePackageTooBig {
                object_size,
                max_object_size,
            } => {
                self.u8(5);
                self.u64(*object_size);
                self.u64(*max_object_size);
            }
            E::CircularObjectOwnership { object } => {
                self.u8(6);
                self.raw(&object.0);
            }
            E::InsufficientCoinBalance => self.u8(7),
            E::CoinBalanceOverflow => self.u8(8),
            E::PublishErrorNonZeroAddress => self.u8(9),
            E::SuiMoveVerificationError => self.u8(10),
            E::MovePrimitiveRuntimeError(location) => {
                self.u8(11);
                match location {
                    Some(l) => {
                        self.u8(1);
                        self.move_location(l);
                    }
                    None => self.u8(0),
                }
            }
            E::MoveAbort(location, code) => {
                self.u8(12);
                self.move_location(location);
                self.u64(*code);
            }
            E::VMVerificationOrDeserializationError => self.u8(13),
            E::VMInvariantViolation => self.u8(14),
            E::FunctionNotFound => self.u8(15),
            E::ArityMismatch => self.u8(16),
            E::TypeArityMismatch => self.u8(17),
            E::NonEntryFunctionInvoked => self.u8(18),
            E::CommandArgumentError { arg_idx, kind } => {
                self.u8(19);
                self.u16(*arg_idx);
                self.command_argument_error(*kind);
            }
            E::TypeArgumentError { argument_idx, kind } => {
                self.u8(20);
                self.u16(*argument_idx);
                self.u8(match kind {
                    TypeArgumentError::TypeNotFound => 0,
                    TypeArgumentError::ConstraintNotSatisfied => 1,
                });
            }
            E::UnusedValueWithoutDrop {
                result_idx,
                secondary_idx,
            } => {
                self.u8(21);
                self.u16(*result_idx);
                self.u16(*secondary_idx);
            }
            E::InvalidPublicFunctionReturnType { idx } => {
                self.u8(22);
                self.u16(*idx);
            }
            E::InvalidTransferObject => self.u8(23),
            E::EffectsTooLarge {
                current_size,
                max_size,
            } => {
                self.u8(24);
                self.u64(*current_size);
                self.u64(*max_size);
            }
            E::PublishUpgradeMissingDependency => self.u8(25),
            E::PublishUpgradeDependencyDowngrade => self.u8(26),
            E::PackageUpgradeError { upgrade_error } => {
                self.u8(27);
                self.package_upgrade_error(upgrade_error);
            }
            E::WrittenObjectsTooLarge {
                current_size,
                max_size,
            } => {
                self.u8(28);
                self.u64(*current_size);
                self.u64(*max_size);
            }
            E::CertificateDenied => self.u8(29),
            E::SuiMoveVerificationTimedout => self.u8(30),
            E::SharedObjectOperationNotAllowed => self.u8(31),
            E::InputObjectDeleted => self.u8(32),
            E::ExecutionCancelledDueToSharedObjectCongestion { congested_objects } => {
                self.u8(33);
                self.len_prefix(congested_objects.len());
                for id in *congested_objects {
                    self.raw(&id.0);
                }
            }
            E::AddressDeniedForCoin { address, coin_type } => {
                self.u8(34);
                self.raw(&address.0);
                self.str(coin_type);
            }
            E::CoinTypeGlobalPause { coin_type } => {
                self.u8(35);
                self.str(coin_type);
            }
            E::ExecutionCancelledDueToRandomnessUnavailable => self.u8(36),
            E::MoveVectorElemTooBig {
                value_size,
                max_scaled_size,
            } => {
                self.u8(37);
                self.u64(*value_size);
                self.u64(*max_scaled_size);
            }
            E::MoveRawValueTooBig {
                value_size,
                max_scaled_size,
            } => {
                self.u8(38);
                self.u64(*value_size);
                self.u64(*max_scaled_size);
            }
            E::InvalidLinkage => self.u8(39),
            E::InsufficientFundsForWithdraw => self.u8(40),
            E::NonExclusiveWriteInputObjectModified { id } => {
                self.u8(41);
                self.raw(&id.0);
            }
        }
    }

    fn command_argument_error(&mut self, kind: CommandArgumentError) {
        use CommandArgumentError as E;
        match kind {
            E::TypeMismatch => self.u8(0),
            E::InvalidBCSBytes => self.u8(1),
            E::InvalidUsageOfPureArg => self.u8(2),
            E::InvalidArgumentToPrivateEntryFunction => self.u8(3),
            E::IndexOutOfBounds { idx } => {
                self.u8(4);
                self.u16(idx);
            }
            E::SecondaryIndexOutOfBounds {
                result_idx,
                secondary_idx,
            } => {
                self.u8(5);
                self.u16(result_idx);
                self.u16(secondary_idx);
            }
            E::InvalidResultArity { result_idx } => {
                self.u8(6);
                self.u16(result_idx);
            }
            E::InvalidGasCoinUsage => self.u8(7),
            E::InvalidValueUsage => self.u8(8),
            E::InvalidObjectByValue => self.u8(9),
            E::InvalidObjectByMutRef => self.u8(10),
            E::SharedObjectOperationNotAllowed => self.u8(11),
            E::InvalidArgumentArity => self.u8(12),
            E::InvalidTransferObject => self.u8(13),
            E::InvalidMakeMoveVecNonObjectArgument => self.u8(14),
            E::ArgumentWithoutValue => self.u8(15),
            E::CannotMoveBorrowedValue => self.u8(16),
            E::CannotWriteToExtendedReference => self.u8(17),
            E::InvalidReferenceArgument => self.u8(18),
            E::InvalidTxContext => self.u8(19),
        }
    }

    fn package_upgrade_error(&mut self, error: &PackageUpgradeError<'_>) {
        use PackageUpgradeError as E;
        match error {
            E::UnableToFetchPackage { package_id } => {
                self.u8(0);
                self.raw(&package_id.0);
            }
            E::NotAPackage { object_id } => {
                self.u8(1);
                self.raw(&object_id.0);
            }
            E::IncompatibleUpgrade => self.u8(2),
            E::DigestDoesNotMatch { digest } => {
                self.u8(3);
                self.bytes(digest);
            }
            E::UnknownUpgradePolicy { policy } => {
                self.u8(4);
                self.u8(*policy);
            }
            E::PackageIDDoesNotMatch {
                package_id,
                ticket_id,
            } => {
                self.u8(5);
                self.raw(&package_id.0);
                self.raw(&ticket_id.0);
            }
        }
    }

    #[inline]
    pub fn address(&mut self, a: &SuiAddress) {
        self.raw(&a.0);
    }

    #[inline]
    pub fn account(&mut self, a: &AccountAddress) {
        self.raw(&a.0);
    }
}
