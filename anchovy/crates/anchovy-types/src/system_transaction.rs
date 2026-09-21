// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The transaction kinds that validators create themselves.

use crate::arena::{Alloc, Ref};
use crate::base::{
    AdditionalConsensusStateDigest, AuthorityPublicKeyBytes, ChainIdentifier,
    ConsensusCommitDigest, ObjectId, ObjectKey, SequenceNumber, TransactionDigest, U32Le, U64Le,
};
use crate::error::{ParseError, Result};
use crate::reader::{Reader, WireRecord};
use crate::transaction::parse_byte_vecs;
use crate::type_tag::TypeInput;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemPackage<'a> {
    pub version: SequenceNumber,
    pub modules: &'a [&'a [u8]],
    pub dependencies: &'a [ObjectId],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeEpoch<'a> {
    pub epoch: u64,
    pub protocol_version: u64,
    pub storage_charge: u64,
    pub computation_charge: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
    pub epoch_start_timestamp_ms: u64,
    pub system_packages: &'a [SystemPackage<'a>],
}

impl<'a> ChangeEpoch<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<ChangeEpoch<'a>> {
        r.enter()?;
        let epoch = r.u64()?;
        let protocol_version = r.u64()?;
        let storage_charge = r.u64()?;
        let computation_charge = r.u64()?;
        let storage_rebate = r.u64()?;
        let non_refundable_storage_fee = r.u64()?;
        let epoch_start_timestamp_ms = r.u64()?;

        // A version and two lengths.
        let n = r.seq_len(8 + 1 + 1)?;
        let mut system_packages = a.slice(n)?;
        for _ in 0..n {
            system_packages.push(SystemPackage {
                version: r.u64()?,
                modules: parse_byte_vecs(r, a)?,
                dependencies: r.record_vec()?,
            });
        }
        let system_packages = system_packages.finish();

        r.leave();
        Ok(ChangeEpoch {
            epoch,
            protocol_version,
            storage_charge,
            computation_charge,
            storage_rebate,
            non_refundable_storage_fee,
            epoch_start_timestamp_ms,
            system_packages,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsensusCommitPrologue {
    pub epoch: u64,
    pub round: u64,
    pub commit_timestamp_ms: u64,
}

impl ConsensusCommitPrologue {
    pub fn parse(r: &mut Reader<'_>) -> Result<ConsensusCommitPrologue> {
        Ok(ConsensusCommitPrologue {
            epoch: r.u64()?,
            round: r.u64()?,
            commit_timestamp_ms: r.u64()?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsensusCommitPrologueV2<'a> {
    pub epoch: u64,
    pub round: u64,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: &'a ConsensusCommitDigest,
}

impl<'a> ConsensusCommitPrologueV2<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<ConsensusCommitPrologueV2<'a>> {
        Ok(ConsensusCommitPrologueV2 {
            epoch: r.u64()?,
            round: r.u64()?,
            commit_timestamp_ms: r.u64()?,
            consensus_commit_digest: ConsensusCommitDigest::parse(r)?,
        })
    }
}

/// An object of a cancelled transaction and the version assigned to it.
/// `start_version` tells apart the successive consensus streams of one id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ConsensusObjectVersion {
    pub id: ObjectId,
    pub start_version: U64Le,
    pub version: U64Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for ConsensusObjectVersion {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsensusDeterminedVersionAssignments<'a> {
    CancelledTransactions(&'a [(&'a TransactionDigest, &'a [ObjectKey])]),
    CancelledTransactionsV2(&'a [(&'a TransactionDigest, &'a [ConsensusObjectVersion])]),
}

impl<'a> ConsensusDeterminedVersionAssignments<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ConsensusDeterminedVersionAssignments<'a>> {
        // A digest and a length.
        const MIN_ENTRY: usize = 33 + 1;
        r.enter()?;
        let assignments = match r.variant()? {
            0 => {
                let n = r.seq_len(MIN_ENTRY)?;
                let mut out = a.slice(n)?;
                for _ in 0..n {
                    out.push((TransactionDigest::parse(r)?, r.record_vec()?));
                }
                ConsensusDeterminedVersionAssignments::CancelledTransactions(out.finish())
            }
            1 => {
                let n = r.seq_len(MIN_ENTRY)?;
                let mut out = a.slice(n)?;
                for _ in 0..n {
                    out.push((TransactionDigest::parse(r)?, r.record_vec()?));
                }
                ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(out.finish())
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ConsensusDeterminedVersionAssignments",
                    tag,
                });
            }
        };
        r.leave();
        Ok(assignments)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsensusCommitPrologueV3<'a> {
    pub epoch: u64,
    pub round: u64,
    pub sub_dag_index: Option<u64>,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: &'a ConsensusCommitDigest,
    pub consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments<'a>,
}

impl<'a> ConsensusCommitPrologueV3<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ConsensusCommitPrologueV3<'a>> {
        r.enter()?;
        let prologue = ConsensusCommitPrologueV3 {
            epoch: r.u64()?,
            round: r.u64()?,
            sub_dag_index: r.option_u64()?,
            commit_timestamp_ms: r.u64()?,
            consensus_commit_digest: ConsensusCommitDigest::parse(r)?,
            consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments::parse(
                r, a,
            )?,
        };
        r.leave();
        Ok(prologue)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsensusCommitPrologueV4<'a> {
    pub epoch: u64,
    pub round: u64,
    pub sub_dag_index: Option<u64>,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: &'a ConsensusCommitDigest,
    pub consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments<'a>,
    pub additional_state_digest: &'a AdditionalConsensusStateDigest,
}

impl<'a> ConsensusCommitPrologueV4<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ConsensusCommitPrologueV4<'a>> {
        r.enter()?;
        let prologue = ConsensusCommitPrologueV4 {
            epoch: r.u64()?,
            round: r.u64()?,
            sub_dag_index: r.option_u64()?,
            commit_timestamp_ms: r.u64()?,
            consensus_commit_digest: ConsensusCommitDigest::parse(r)?,
            consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments::parse(
                r, a,
            )?,
            additional_state_digest: AdditionalConsensusStateDigest::parse(r)?,
        };
        r.leave();
        Ok(prologue)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JwkId<'a> {
    pub iss: &'a str,
    pub kid: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Jwk<'a> {
    pub kty: &'a str,
    pub e: &'a str,
    pub n: &'a str,
    pub alg: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActiveJwk<'a> {
    pub jwk_id: JwkId<'a>,
    pub jwk: Jwk<'a>,
    pub epoch: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthenticatorStateUpdate<'a> {
    pub epoch: u64,
    pub round: u64,
    pub new_active_jwks: &'a [ActiveJwk<'a>],
    pub authenticator_obj_initial_shared_version: SequenceNumber,
}

impl<'a> AuthenticatorStateUpdate<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<AuthenticatorStateUpdate<'a>> {
        r.enter()?;
        let epoch = r.u64()?;
        let round = r.u64()?;

        // Six lengths and an epoch.
        let n = r.seq_len(6 + 8)?;
        let mut new_active_jwks = a.slice(n)?;
        for _ in 0..n {
            new_active_jwks.push(ActiveJwk {
                jwk_id: JwkId {
                    iss: r.str()?,
                    kid: r.str()?,
                },
                jwk: Jwk {
                    kty: r.str()?,
                    e: r.str()?,
                    n: r.str()?,
                    alg: r.str()?,
                },
                epoch: r.u64()?,
            });
        }
        let new_active_jwks = new_active_jwks.finish();

        let authenticator_obj_initial_shared_version = r.u64()?;
        r.leave();
        Ok(AuthenticatorStateUpdate {
            epoch,
            round,
            new_active_jwks,
            authenticator_obj_initial_shared_version,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RandomnessStateUpdate<'a> {
    pub epoch: u64,
    pub randomness_round: u64,
    pub random_bytes: &'a [u8],
    pub randomness_obj_initial_shared_version: SequenceNumber,
}

impl<'a> RandomnessStateUpdate<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<RandomnessStateUpdate<'a>> {
        Ok(RandomnessStateUpdate {
            epoch: r.u64()?,
            randomness_round: r.u64()?,
            random_bytes: r.byte_vec()?,
            randomness_obj_initial_shared_version: r.u64()?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecutionTimeObservationKey<'a> {
    MoveEntryPoint {
        package: &'a ObjectId,
        module: &'a str,
        function: &'a str,
        type_arguments: &'a [TypeInput<'a>],
    },
    TransferObjects,
    SplitCoins,
    MergeCoins,
    Publish,
    MakeMoveVec,
    Upgrade,
}

impl<'a> ExecutionTimeObservationKey<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ExecutionTimeObservationKey<'a>> {
        r.enter()?;
        let key = match r.variant()? {
            0 => ExecutionTimeObservationKey::MoveEntryPoint {
                package: ObjectId::parse(r)?,
                module: r.str()?,
                function: r.str()?,
                type_arguments: TypeInput::parse_vec(r, a)?,
            },
            1 => ExecutionTimeObservationKey::TransferObjects,
            2 => ExecutionTimeObservationKey::SplitCoins,
            3 => ExecutionTimeObservationKey::MergeCoins,
            4 => ExecutionTimeObservationKey::Publish,
            5 => ExecutionTimeObservationKey::MakeMoveVec,
            6 => ExecutionTimeObservationKey::Upgrade,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ExecutionTimeObservationKey",
                    tag,
                });
            }
        };
        r.leave();
        Ok(key)
    }
}

/// One authority's measurement: its name and a `Duration`. Whether the
/// duration is representable is not checked here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct AuthorityObservation {
    pub authority: AuthorityPublicKeyBytes,
    pub secs: U64Le,
    pub nanos: U32Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for AuthorityObservation {}

/// `StoredExecutionTimeObservations::V1`, the only version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoredExecutionTimeObservations<'a>(
    pub &'a [(ExecutionTimeObservationKey<'a>, &'a [AuthorityObservation])],
);

impl<'a> StoredExecutionTimeObservations<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<StoredExecutionTimeObservations<'a>> {
        r.enter()?;
        match r.variant()? {
            0 => {}
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "StoredExecutionTimeObservations",
                    tag,
                });
            }
        }
        // A unit key and a length.
        let n = r.seq_len(1 + 1)?;
        let mut out = a.slice(n)?;
        for _ in 0..n {
            let key = ExecutionTimeObservationKey::parse(r, a)?;
            let observations: &[AuthorityObservation] = r.record_vec()?;
            for o in observations {
                o.authority.check()?;
            }
            out.push((key, observations));
        }
        r.leave();
        Ok(StoredExecutionTimeObservations(out.finish()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndOfEpochTransactionKind<'a> {
    ChangeEpoch(Ref<'a, ChangeEpoch<'a>>),
    AuthenticatorStateCreate,
    AuthenticatorStateExpire {
        min_epoch: u64,
        authenticator_obj_initial_shared_version: SequenceNumber,
    },
    RandomnessStateCreate,
    DenyListStateCreate,
    BridgeStateCreate(&'a ChainIdentifier),
    BridgeCommitteeInit(SequenceNumber),
    StoreExecutionTimeObservations(StoredExecutionTimeObservations<'a>),
    AccumulatorRootCreate,
    CoinRegistryCreate,
    DisplayRegistryCreate,
    AddressAliasStateCreate,
    WriteAccumulatorStorageCost {
        storage_cost: u64,
    },
    ForwardingAddressRegistryCreate,
}

impl<'a> EndOfEpochTransactionKind<'a> {
    pub const MIN_WIRE_SIZE: usize = 1;

    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<EndOfEpochTransactionKind<'a>> {
        r.enter()?;
        let kind = match r.variant()? {
            0 => {
                let v = ChangeEpoch::parse(r, a)?;
                EndOfEpochTransactionKind::ChangeEpoch(a.value(v)?)
            }
            1 => EndOfEpochTransactionKind::AuthenticatorStateCreate,
            2 => EndOfEpochTransactionKind::AuthenticatorStateExpire {
                min_epoch: r.u64()?,
                authenticator_obj_initial_shared_version: r.u64()?,
            },
            3 => EndOfEpochTransactionKind::RandomnessStateCreate,
            4 => EndOfEpochTransactionKind::DenyListStateCreate,
            5 => EndOfEpochTransactionKind::BridgeStateCreate(ChainIdentifier::parse(r)?),
            6 => EndOfEpochTransactionKind::BridgeCommitteeInit(r.u64()?),
            7 => EndOfEpochTransactionKind::StoreExecutionTimeObservations(
                StoredExecutionTimeObservations::parse(r, a)?,
            ),
            8 => EndOfEpochTransactionKind::AccumulatorRootCreate,
            9 => EndOfEpochTransactionKind::CoinRegistryCreate,
            10 => EndOfEpochTransactionKind::DisplayRegistryCreate,
            11 => EndOfEpochTransactionKind::AddressAliasStateCreate,
            12 => EndOfEpochTransactionKind::WriteAccumulatorStorageCost {
                storage_cost: r.u64()?,
            },
            13 => EndOfEpochTransactionKind::ForwardingAddressRegistryCreate,
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "EndOfEpochTransactionKind",
                    tag,
                });
            }
        };
        r.leave();
        Ok(kind)
    }
}

crate::base::assert_wire_layout!(ConsensusObjectVersion = 48, AuthorityObservation = 109);
