// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{
    AuthorityPublicKeyBytes, Digest, ObjectId, ObjectRef, ProtocolVersion, SequenceNumber,
    SuiAddress,
};
use super::checkpoint::CheckpointSummary;
use super::signature::AuthorityQuorumSignInfo;
use super::system_transaction::{Duration, ExecutionTimeObservationKey, Jwk, JwkId};
use super::transaction::{SenderSignedData, Transaction};
use crate::consensus as view;

/// `Envelope<SenderSignedData, AuthorityStrongQuorumSignInfo>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CertifiedTransaction {
    pub data: SenderSignedData,
    pub auth_signature: AuthorityQuorumSignInfo,
}

/// A BLS signature, opaque.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthoritySignInfo {
    pub epoch: u64,
    pub authority: AuthorityPublicKeyBytes,
    #[serde(with = "super::byte_array")]
    pub signature: [u8; 48],
}

/// `Envelope<CheckpointSummary, AuthoritySignInfo>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SignedCheckpointSummary {
    pub data: CheckpointSummary,
    pub auth_signature: AuthoritySignInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointSignatureMessage {
    pub summary: SignedCheckpointSummary,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupportedProtocolVersions {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SupportedProtocolVersionsWithHashes {
    pub versions: Vec<(ProtocolVersion, Digest)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthorityCapabilitiesV1 {
    pub authority: AuthorityPublicKeyBytes,
    pub generation: u64,
    pub supported_protocol_versions: SupportedProtocolVersions,
    pub available_system_packages: Vec<ObjectRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthorityCapabilitiesV2 {
    pub authority: AuthorityPublicKeyBytes,
    pub generation: u64,
    pub supported_protocol_versions: SupportedProtocolVersionsWithHashes,
    pub available_system_packages: Vec<ObjectRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExecutionTimeObservation {
    pub authority: AuthorityPublicKeyBytes,
    pub generation: u64,
    pub estimates: Vec<(ExecutionTimeObservationKey, Duration)>,
}

/// The lists may be empty here; the reference rejects that.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TransactionClaim {
    AddressAliases(Vec<(SuiAddress, Option<SequenceNumber>)>),
    ImmutableInputObjects(Vec<ObjectId>),
    AddressAliasesV2(Vec<(u8, Option<SequenceNumber>)>),
}

/// `TransactionWithClaims<Transaction>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PlainTransactionWithClaims {
    pub tx: Transaction,
    pub claims: Vec<TransactionClaim>,
}

/// The reference's sets, as sequences in wire order, which it does not
/// check.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
// The reference's layout.
#[allow(clippy::struct_excessive_bools)]
pub struct TransactionDenyRules {
    pub object_deny_list: Vec<ObjectId>,
    pub package_deny_list: Vec<ObjectId>,
    pub address_deny_list: Vec<SuiAddress>,
    pub package_publish_disabled: bool,
    pub package_upgrade_disabled: bool,
    pub shared_object_disabled: bool,
    pub user_transaction_disabled: bool,
    pub gasless_disabled: bool,
    pub receiving_objects_disabled: bool,
    pub zklogin_sig_disabled: bool,
    pub zklogin_disabled_providers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SharedTransactionDenyConfigV1 {
    pub authority: AuthorityPublicKeyBytes,
    pub generation: u64,
    pub rules: Option<TransactionDenyRules>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SharedTransactionDenyConfig {
    V1(SharedTransactionDenyConfigV1),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ConsensusTransactionKind {
    CertifiedTransaction(CertifiedTransaction),
    CheckpointSignature(CheckpointSignatureMessage),
    EndOfPublish(AuthorityPublicKeyBytes),
    CapabilityNotification(AuthorityCapabilitiesV1),
    #[serde(rename = "NewJWKFetched")]
    NewJwkFetched(AuthorityPublicKeyBytes, JwkId, Jwk),
    RandomnessStateUpdate(u64, Vec<u8>),
    RandomnessDkgMessage(AuthorityPublicKeyBytes, Vec<u8>),
    RandomnessDkgConfirmation(AuthorityPublicKeyBytes, Vec<u8>),
    CapabilityNotificationV2(AuthorityCapabilitiesV2),
    UserTransaction(Transaction),
    ExecutionTimeObservation(ExecutionTimeObservation),
    CheckpointSignatureV2(CheckpointSignatureMessage),
    UserTransactionV2(PlainTransactionWithClaims),
    UpdateTransactionDenyConfig(SharedTransactionDenyConfig),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsensusTransaction {
    pub tracking_id: [u8; 8],
    pub kind: ConsensusTransactionKind,
}

impl From<&view::CertifiedTransaction<'_>> for CertifiedTransaction {
    fn from(v: &view::CertifiedTransaction<'_>) -> Self {
        CertifiedTransaction {
            data: SenderSignedData::from(&v.data),
            auth_signature: AuthorityQuorumSignInfo::from(&v.auth_signature),
        }
    }
}

impl From<&view::AuthoritySignInfo<'_>> for AuthoritySignInfo {
    fn from(v: &view::AuthoritySignInfo<'_>) -> Self {
        AuthoritySignInfo {
            epoch: v.epoch,
            authority: AuthorityPublicKeyBytes::from(v.authority),
            signature: *v.signature,
        }
    }
}

impl From<&view::SignedCheckpointSummary<'_>> for SignedCheckpointSummary {
    fn from(v: &view::SignedCheckpointSummary<'_>) -> Self {
        SignedCheckpointSummary {
            data: CheckpointSummary::from(&v.data),
            auth_signature: AuthoritySignInfo::from(&v.auth_signature),
        }
    }
}

impl From<&view::CheckpointSignatureMessage<'_>> for CheckpointSignatureMessage {
    fn from(v: &view::CheckpointSignatureMessage<'_>) -> Self {
        CheckpointSignatureMessage {
            summary: SignedCheckpointSummary::from(&v.summary),
        }
    }
}

impl From<&view::SupportedProtocolVersions> for SupportedProtocolVersions {
    fn from(v: &view::SupportedProtocolVersions) -> Self {
        SupportedProtocolVersions {
            min: ProtocolVersion(v.min),
            max: ProtocolVersion(v.max),
        }
    }
}

impl From<&view::ProtocolVersionDigest> for (ProtocolVersion, Digest) {
    fn from(v: &view::ProtocolVersionDigest) -> Self {
        (ProtocolVersion(v.version.get()), Digest::from(&v.digest))
    }
}

impl From<&view::AuthorityCapabilitiesV1<'_>> for AuthorityCapabilitiesV1 {
    fn from(v: &view::AuthorityCapabilitiesV1<'_>) -> Self {
        AuthorityCapabilitiesV1 {
            authority: AuthorityPublicKeyBytes::from(v.authority),
            generation: v.generation,
            supported_protocol_versions: SupportedProtocolVersions::from(
                &v.supported_protocol_versions,
            ),
            available_system_packages: v.available_system_packages.iter().map(Into::into).collect(),
        }
    }
}

impl From<&view::AuthorityCapabilitiesV2<'_>> for AuthorityCapabilitiesV2 {
    fn from(v: &view::AuthorityCapabilitiesV2<'_>) -> Self {
        AuthorityCapabilitiesV2 {
            authority: AuthorityPublicKeyBytes::from(v.authority),
            generation: v.generation,
            supported_protocol_versions: SupportedProtocolVersionsWithHashes {
                versions: v
                    .supported_protocol_versions
                    .iter()
                    .map(Into::into)
                    .collect(),
            },
            available_system_packages: v.available_system_packages.iter().map(Into::into).collect(),
        }
    }
}

impl From<&view::Duration> for Duration {
    fn from(v: &view::Duration) -> Self {
        Duration {
            secs: v.secs,
            nanos: v.nanos,
        }
    }
}

impl From<&view::ExecutionTimeObservation<'_>> for ExecutionTimeObservation {
    fn from(v: &view::ExecutionTimeObservation<'_>) -> Self {
        ExecutionTimeObservation {
            authority: AuthorityPublicKeyBytes::from(v.authority),
            generation: v.generation,
            estimates: v
                .estimates
                .iter()
                .map(|(key, duration)| {
                    (
                        ExecutionTimeObservationKey::from(key),
                        Duration::from(duration),
                    )
                })
                .collect(),
        }
    }
}

impl From<&view::TransactionClaim<'_>> for TransactionClaim {
    fn from(v: &view::TransactionClaim<'_>) -> Self {
        match v {
            view::TransactionClaim::AddressAliases(aliases) => TransactionClaim::AddressAliases(
                aliases
                    .iter()
                    .map(|(address, version)| {
                        (SuiAddress::from(*address), version.map(SequenceNumber))
                    })
                    .collect(),
            ),
            view::TransactionClaim::ImmutableInputObjects(ids) => {
                TransactionClaim::ImmutableInputObjects(ids.iter().map(ObjectId::from).collect())
            }
            view::TransactionClaim::AddressAliasesV2(aliases) => {
                TransactionClaim::AddressAliasesV2(
                    aliases
                        .iter()
                        .map(|(index, version)| (*index, version.map(SequenceNumber)))
                        .collect(),
                )
            }
        }
    }
}

impl From<&view::PlainTransactionWithClaims<'_>> for PlainTransactionWithClaims {
    fn from(v: &view::PlainTransactionWithClaims<'_>) -> Self {
        PlainTransactionWithClaims {
            tx: Transaction::from(&v.transaction().0),
            claims: v.claims().iter().map(TransactionClaim::from).collect(),
        }
    }
}

impl From<&view::TransactionDenyRules<'_>> for TransactionDenyRules {
    fn from(v: &view::TransactionDenyRules<'_>) -> Self {
        TransactionDenyRules {
            object_deny_list: v.object_deny_list.iter().map(ObjectId::from).collect(),
            package_deny_list: v.package_deny_list.iter().map(ObjectId::from).collect(),
            address_deny_list: v.address_deny_list.iter().map(SuiAddress::from).collect(),
            package_publish_disabled: v.package_publish_disabled,
            package_upgrade_disabled: v.package_upgrade_disabled,
            shared_object_disabled: v.shared_object_disabled,
            user_transaction_disabled: v.user_transaction_disabled,
            gasless_disabled: v.gasless_disabled,
            receiving_objects_disabled: v.receiving_objects_disabled,
            zklogin_sig_disabled: v.zklogin_sig_disabled,
            zklogin_disabled_providers: v
                .zklogin_disabled_providers
                .iter()
                .map(|provider| (*provider).to_owned())
                .collect(),
        }
    }
}

impl From<&view::SharedTransactionDenyConfig<'_>> for SharedTransactionDenyConfig {
    fn from(v: &view::SharedTransactionDenyConfig<'_>) -> Self {
        SharedTransactionDenyConfig::V1(SharedTransactionDenyConfigV1 {
            authority: AuthorityPublicKeyBytes::from(v.authority),
            generation: v.generation,
            rules: v.rules.as_ref().map(TransactionDenyRules::from),
        })
    }
}

impl From<&view::ConsensusTransactionKind<'_>> for ConsensusTransactionKind {
    fn from(v: &view::ConsensusTransactionKind<'_>) -> Self {
        use ConsensusTransactionKind as B;
        use view::ConsensusTransactionKind as V;
        let name =
            |name: &crate::base::AuthorityPublicKeyBytes| AuthorityPublicKeyBytes::from(name);
        match v {
            V::CertifiedTransaction(cert) => {
                B::CertifiedTransaction(CertifiedTransaction::from(&**cert))
            }
            V::CheckpointSignature(message) => {
                B::CheckpointSignature(CheckpointSignatureMessage::from(&**message))
            }
            V::EndOfPublish(authority) => B::EndOfPublish(name(authority)),
            V::CapabilityNotification(capabilities) => {
                B::CapabilityNotification(AuthorityCapabilitiesV1::from(capabilities))
            }
            V::NewJwkFetched(fetched) => B::NewJwkFetched(
                name(fetched.authority),
                JwkId::from(&fetched.jwk_id),
                Jwk::from(&fetched.jwk),
            ),
            V::RandomnessStateUpdate { round, bytes } => {
                B::RandomnessStateUpdate(*round, bytes.to_vec())
            }
            V::RandomnessDkgMessage(dkg) => {
                B::RandomnessDkgMessage(name(dkg.authority), dkg.bytes.to_vec())
            }
            V::RandomnessDkgConfirmation(dkg) => {
                B::RandomnessDkgConfirmation(name(dkg.authority), dkg.bytes.to_vec())
            }
            V::CapabilityNotificationV2(capabilities) => {
                B::CapabilityNotificationV2(AuthorityCapabilitiesV2::from(capabilities))
            }
            V::UserTransaction(tx) => B::UserTransaction(Transaction::from(&tx.0)),
            V::ExecutionTimeObservation(observation) => {
                B::ExecutionTimeObservation(ExecutionTimeObservation::from(observation))
            }
            V::CheckpointSignatureV2(message) => {
                B::CheckpointSignatureV2(CheckpointSignatureMessage::from(&**message))
            }
            V::UserTransactionV2(tx) => {
                B::UserTransactionV2(PlainTransactionWithClaims::from(&**tx))
            }
            V::UpdateTransactionDenyConfig(config) => {
                B::UpdateTransactionDenyConfig(SharedTransactionDenyConfig::from(&**config))
            }
        }
    }
}

impl From<&view::ConsensusTransaction<'_>> for ConsensusTransaction {
    fn from(v: &view::ConsensusTransaction<'_>) -> Self {
        ConsensusTransaction {
            tracking_id: *v.tracking_id(),
            kind: ConsensusTransactionKind::from(v.kind()),
        }
    }
}
