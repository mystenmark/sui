// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ConsensusTransaction`, what validators submit to consensus and vote on.
//!
//! None of these types are in sui's format snapshot; the build mirrors are
//! checked against sui-types' encoding instead (`sui-oracle
//! --consensus-vectors`).

use crate::arena::{Alloc, Ref};
use crate::base::{AuthorityPublicKeyBytes, Digest, ObjectId, ObjectRef, SuiAddress, U64Le};
use crate::checkpoint::{AuthorityQuorumSignInfo, CheckpointSummary};
use crate::error::{ParseError, Result};
use crate::reader::{Reader, WireRecord};
use crate::system_transaction::{ExecutionTimeObservationKey, Jwk, JwkId};
use crate::transaction::{DigestPending, SenderSignedData, Transaction};

fn authority<'a>(r: &mut Reader<'a>) -> Result<&'a AuthorityPublicKeyBytes> {
    let name: &AuthorityPublicKeyBytes = r.record()?;
    name.check()?;
    Ok(name)
}

/// The reference's `CertifiedTransaction`: `Envelope<SenderSignedData,
/// AuthorityStrongQuorumSignInfo>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CertifiedTransaction<'a> {
    pub data: SenderSignedData<'a, DigestPending>,
    pub auth_signature: AuthorityQuorumSignInfo<'a>,
}

impl<'a> CertifiedTransaction<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<CertifiedTransaction<'a>> {
        r.enter()?;
        let data = SenderSignedData::parse(r, a)?;
        r.enter()?;
        let auth_signature = AuthorityQuorumSignInfo {
            epoch: r.u64()?,
            signature: r.array()?,
            signers_map: r.byte_vec()?,
        };
        r.leave();
        r.leave();
        Ok(CertifiedTransaction {
            data,
            auth_signature,
        })
    }
}

/// One authority's BLS signature, not checked to be a curve point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthoritySignInfo<'a> {
    pub epoch: u64,
    pub authority: &'a AuthorityPublicKeyBytes,
    pub signature: &'a [u8; 48],
}

/// `Envelope<CheckpointSummary, AuthoritySignInfo>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SignedCheckpointSummary<'a> {
    pub data: CheckpointSummary<'a>,
    pub auth_signature: AuthoritySignInfo<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointSignatureMessage<'a> {
    pub summary: SignedCheckpointSummary<'a>,
}

impl<'a> CheckpointSignatureMessage<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<CheckpointSignatureMessage<'a>> {
        r.enter()?;
        r.enter()?;
        let data = CheckpointSummary::parse(r, a)?;
        let auth_signature = AuthoritySignInfo {
            epoch: r.u64()?,
            authority: authority(r)?,
            signature: r.array()?,
        };
        r.leave();
        r.leave();
        Ok(CheckpointSignatureMessage {
            summary: SignedCheckpointSummary {
                data,
                auth_signature,
            },
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SupportedProtocolVersions {
    pub min: u64,
    pub max: u64,
}

/// A `SupportedProtocolVersionsWithHashes` entry: a protocol version and
/// the digest of its config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ProtocolVersionDigest {
    pub version: U64Le,
    pub digest: Digest,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for ProtocolVersionDigest {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthorityCapabilitiesV1<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub generation: u64,
    pub supported_protocol_versions: SupportedProtocolVersions,
    pub available_system_packages: &'a [ObjectRef],
}

impl<'a> AuthorityCapabilitiesV1<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<AuthorityCapabilitiesV1<'a>> {
        r.enter()?;
        let capabilities = AuthorityCapabilitiesV1 {
            authority: authority(r)?,
            generation: r.u64()?,
            supported_protocol_versions: SupportedProtocolVersions {
                min: r.u64()?,
                max: r.u64()?,
            },
            available_system_packages: ObjectRef::parse_vec(r)?,
        };
        r.leave();
        Ok(capabilities)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthorityCapabilitiesV2<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub generation: u64,
    pub supported_protocol_versions: &'a [ProtocolVersionDigest],
    pub available_system_packages: &'a [ObjectRef],
}

impl<'a> AuthorityCapabilitiesV2<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<AuthorityCapabilitiesV2<'a>> {
        r.enter()?;
        let authority = authority(r)?;
        let generation = r.u64()?;
        let supported_protocol_versions: &[ProtocolVersionDigest] = r.record_vec()?;
        for v in supported_protocol_versions {
            v.digest.check()?;
        }
        let available_system_packages = ObjectRef::parse_vec(r)?;
        r.leave();
        Ok(AuthorityCapabilitiesV2 {
            authority,
            generation,
            supported_protocol_versions,
            available_system_packages,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NewJwkFetched<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub jwk_id: JwkId<'a>,
    pub jwk: Jwk<'a>,
}

/// A `RandomnessDkgMessage` or `RandomnessDkgConfirmation`: its sender and
/// a serialized `VersionedDkgMessage` or `VersionedDkgConfirmation`, not
/// decoded here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RandomnessDkg<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub bytes: &'a [u8],
}

impl RandomnessDkg<'_> {
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

/// `std::time::Duration` as serde writes it. `nanos` is not range-checked,
/// and the reference rejects a duration that overflows when normalized.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Duration {
    pub secs: u64,
    pub nanos: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecutionTimeObservation<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub generation: u64,
    pub estimates: &'a [(ExecutionTimeObservationKey<'a>, Duration)],
}

impl<'a> ExecutionTimeObservation<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ExecutionTimeObservation<'a>> {
        r.enter()?;
        let authority = authority(r)?;
        let generation = r.u64()?;
        // A unit key and a duration.
        let n = r.seq_len(1 + 12)?;
        let mut estimates = a.slice(n)?;
        for _ in 0..n {
            let key = ExecutionTimeObservationKey::parse(r, a)?;
            estimates.push((
                key,
                Duration {
                    secs: r.u64()?,
                    nanos: r.u32()?,
                },
            ));
        }
        r.leave();
        Ok(ExecutionTimeObservation {
            authority,
            generation,
            estimates: estimates.finish(),
        })
    }

    pub fn estimate_count(&self) -> usize {
        self.estimates.len()
    }
}

/// The `NonEmpty` lists may be empty here; the reference rejects that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransactionClaim<'a> {
    /// Deprecated in favor of `AddressAliasesV2`.
    AddressAliases(&'a [(&'a SuiAddress, Option<u64>)]),
    ImmutableInputObjects(&'a [ObjectId]),
    /// Per required signer: a signature index and an alias object version.
    AddressAliasesV2(&'a [(u8, Option<u64>)]),
}

impl<'a> TransactionClaim<'a> {
    /// A variant index and a length.
    pub const MIN_WIRE_SIZE: usize = 2;

    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionClaim<'a>> {
        r.enter()?;
        let claim = match r.variant()? {
            0 => {
                // An address and an option tag.
                let n = r.seq_len(32 + 1)?;
                let mut out = a.slice(n)?;
                for _ in 0..n {
                    out.push((SuiAddress::parse(r)?, r.option_u64()?));
                }
                TransactionClaim::AddressAliases(out.finish())
            }
            1 => TransactionClaim::ImmutableInputObjects(r.record_vec()?),
            2 => {
                // An index and an option tag.
                let n = r.seq_len(1 + 1)?;
                let mut out = a.slice(n)?;
                for _ in 0..n {
                    out.push((r.u8()?, r.option_u64()?));
                }
                TransactionClaim::AddressAliasesV2(out.finish())
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "TransactionClaim",
                    tag,
                });
            }
        };
        r.leave();
        Ok(claim)
    }
}

/// The reference's `PlainTransactionWithClaims`, a user transaction as
/// submitted to consensus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlainTransactionWithClaims<'a> {
    transaction: Transaction<'a, DigestPending>,
    claims: &'a [TransactionClaim<'a>],
}

impl<'a> PlainTransactionWithClaims<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<PlainTransactionWithClaims<'a>> {
        r.enter()?;
        let transaction = Transaction::parse(r, a)?;
        let n = r.seq_len(TransactionClaim::MIN_WIRE_SIZE)?;
        let mut claims = a.slice(n)?;
        for _ in 0..n {
            claims.push(TransactionClaim::parse(r, a)?);
        }
        r.leave();
        Ok(PlainTransactionWithClaims {
            transaction,
            claims: claims.finish(),
        })
    }

    /// The exact encoding of the `Transaction`, which parses on its own as
    /// a `Transaction`.
    pub fn transaction_bytes(&self) -> &'a [u8] {
        self.transaction.0.bytes()
    }

    pub fn transaction(&self) -> &Transaction<'a, DigestPending> {
        &self.transaction
    }

    pub fn claims(&self) -> &'a [TransactionClaim<'a>] {
        self.claims
    }
}

/// The shareable subset of a node's `TransactionDenyConfig`. The reference
/// decodes the lists into sets, which accept any order and repeats, so they
/// are not checked for either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// The reference's layout.
#[allow(clippy::struct_excessive_bools)]
pub struct TransactionDenyRules<'a> {
    pub object_deny_list: &'a [ObjectId],
    pub package_deny_list: &'a [ObjectId],
    pub address_deny_list: &'a [SuiAddress],
    pub package_publish_disabled: bool,
    pub package_upgrade_disabled: bool,
    pub shared_object_disabled: bool,
    pub user_transaction_disabled: bool,
    pub gasless_disabled: bool,
    pub receiving_objects_disabled: bool,
    pub zklogin_sig_disabled: bool,
    pub zklogin_disabled_providers: &'a [&'a str],
}

impl<'a> TransactionDenyRules<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<TransactionDenyRules<'a>> {
        r.enter()?;
        let object_deny_list = r.record_vec()?;
        let package_deny_list = r.record_vec()?;
        let address_deny_list = r.record_vec()?;
        let package_publish_disabled = r.bool()?;
        let package_upgrade_disabled = r.bool()?;
        let shared_object_disabled = r.bool()?;
        let user_transaction_disabled = r.bool()?;
        let gasless_disabled = r.bool()?;
        let receiving_objects_disabled = r.bool()?;
        let zklogin_sig_disabled = r.bool()?;
        let n = r.seq_len(1)?;
        let mut providers = a.slice(n)?;
        for _ in 0..n {
            providers.push(r.str()?);
        }
        r.leave();
        Ok(TransactionDenyRules {
            object_deny_list,
            package_deny_list,
            address_deny_list,
            package_publish_disabled,
            package_upgrade_disabled,
            shared_object_disabled,
            user_transaction_disabled,
            gasless_disabled,
            receiving_objects_disabled,
            zklogin_sig_disabled,
            zklogin_disabled_providers: providers.finish(),
        })
    }
}

/// `SharedTransactionDenyConfig::V1`, the only version. `rules` of `None`
/// withdraws the authority's earlier recommendation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SharedTransactionDenyConfig<'a> {
    pub authority: &'a AuthorityPublicKeyBytes,
    pub generation: u64,
    pub rules: Option<TransactionDenyRules<'a>>,
}

impl<'a> SharedTransactionDenyConfig<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<SharedTransactionDenyConfig<'a>> {
        r.enter()?;
        match r.variant()? {
            0 => {}
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "SharedTransactionDenyConfig",
                    tag,
                });
            }
        }
        r.enter()?;
        let authority = authority(r)?;
        let generation = r.u64()?;
        let rules = if r.option()? {
            Some(TransactionDenyRules::parse(r, a)?)
        } else {
            None
        };
        r.leave();
        r.leave();
        Ok(SharedTransactionDenyConfig {
            authority,
            generation,
            rules,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsensusTransactionKind<'a> {
    CertifiedTransaction(Ref<'a, CertifiedTransaction<'a>>),
    /// Deprecated in favor of `CheckpointSignatureV2`.
    CheckpointSignature(Ref<'a, CheckpointSignatureMessage<'a>>),
    EndOfPublish(&'a AuthorityPublicKeyBytes),
    /// Deprecated in favor of `CapabilityNotificationV2`.
    CapabilityNotification(AuthorityCapabilitiesV1<'a>),
    NewJwkFetched(NewJwkFetched<'a>),
    /// Deprecated.
    RandomnessStateUpdate {
        round: u64,
        bytes: &'a [u8],
    },
    RandomnessDkgMessage(RandomnessDkg<'a>),
    RandomnessDkgConfirmation(RandomnessDkg<'a>),
    CapabilityNotificationV2(AuthorityCapabilitiesV2<'a>),
    UserTransaction(Ref<'a, Transaction<'a, DigestPending>>),
    ExecutionTimeObservation(ExecutionTimeObservation<'a>),
    CheckpointSignatureV2(Ref<'a, CheckpointSignatureMessage<'a>>),
    UserTransactionV2(Ref<'a, PlainTransactionWithClaims<'a>>),
    UpdateTransactionDenyConfig(Ref<'a, SharedTransactionDenyConfig<'a>>),
}

impl<'a> ConsensusTransactionKind<'a> {
    pub fn parse<A: Alloc<'a>>(
        r: &mut Reader<'a>,
        a: &mut A,
    ) -> Result<ConsensusTransactionKind<'a>> {
        use ConsensusTransactionKind as K;
        r.enter()?;
        let kind = match r.variant()? {
            0 => {
                let v = CertifiedTransaction::parse(r, a)?;
                K::CertifiedTransaction(a.value(v)?)
            }
            1 => {
                let v = CheckpointSignatureMessage::parse(r, a)?;
                K::CheckpointSignature(a.value(v)?)
            }
            2 => K::EndOfPublish(authority(r)?),
            3 => K::CapabilityNotification(AuthorityCapabilitiesV1::parse(r)?),
            4 => K::NewJwkFetched(NewJwkFetched {
                authority: authority(r)?,
                jwk_id: JwkId::parse(r)?,
                jwk: Jwk::parse(r)?,
            }),
            5 => K::RandomnessStateUpdate {
                round: r.u64()?,
                bytes: r.byte_vec()?,
            },
            6 => K::RandomnessDkgMessage(RandomnessDkg {
                authority: authority(r)?,
                bytes: r.byte_vec()?,
            }),
            7 => K::RandomnessDkgConfirmation(RandomnessDkg {
                authority: authority(r)?,
                bytes: r.byte_vec()?,
            }),
            8 => K::CapabilityNotificationV2(AuthorityCapabilitiesV2::parse(r)?),
            9 => {
                let v = Transaction::parse(r, a)?;
                K::UserTransaction(a.value(v)?)
            }
            10 => K::ExecutionTimeObservation(ExecutionTimeObservation::parse(r, a)?),
            11 => {
                let v = CheckpointSignatureMessage::parse(r, a)?;
                K::CheckpointSignatureV2(a.value(v)?)
            }
            12 => {
                let v = PlainTransactionWithClaims::parse(r, a)?;
                K::UserTransactionV2(a.value(v)?)
            }
            13 => {
                let v = SharedTransactionDenyConfig::parse(r, a)?;
                K::UpdateTransactionDenyConfig(a.value(v)?)
            }
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "ConsensusTransactionKind",
                    tag,
                });
            }
        };
        r.leave();
        Ok(kind)
    }

    /// The authority the payload names as its sender, for the kinds that
    /// name one.
    pub fn authority(&self) -> Option<&'a AuthorityPublicKeyBytes> {
        use ConsensusTransactionKind as K;
        match *self {
            K::CheckpointSignature(m) | K::CheckpointSignatureV2(m) => {
                Some(m.get().summary.auth_signature.authority)
            }
            K::EndOfPublish(name) => Some(name),
            K::CapabilityNotification(c) => Some(c.authority),
            K::NewJwkFetched(j) => Some(j.authority),
            K::RandomnessDkgMessage(d) | K::RandomnessDkgConfirmation(d) => Some(d.authority),
            K::CapabilityNotificationV2(c) => Some(c.authority),
            K::ExecutionTimeObservation(o) => Some(o.authority),
            K::UpdateTransactionDenyConfig(c) => Some(c.get().authority),
            K::CertifiedTransaction(_)
            | K::RandomnessStateUpdate { .. }
            | K::UserTransaction(_)
            | K::UserTransactionV2(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsensusTransaction<'a> {
    tracking_id: &'a [u8; 8],
    kind: ConsensusTransactionKind<'a>,
}

impl<'a> ConsensusTransaction<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<ConsensusTransaction<'a>> {
        r.enter()?;
        let tracking_id = r.array()?;
        let kind = ConsensusTransactionKind::parse(r, a)?;
        r.leave();
        Ok(ConsensusTransaction { tracking_id, kind })
    }

    pub fn tracking_id(&self) -> &'a [u8; 8] {
        self.tracking_id
    }

    pub fn kind(&self) -> &ConsensusTransactionKind<'a> {
        &self.kind
    }
}

// Mostly user transactions, whose arena use dominates.
crate::impl_wire!(ConsensusTransaction, guess = 34);

crate::base::assert_wire_layout!(ProtocolVersionDigest = 41);
