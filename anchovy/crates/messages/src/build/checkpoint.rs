// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{
    AuthorityPublicKeyBytes, CheckpointArtifactsDigest, CheckpointContentsDigest, CheckpointDigest,
    Digest, EcmhLiveObjectSetDigest, ProtocolVersion, SequenceNumber, TransactionDigest,
    TransactionEffectsDigest,
};
use super::effects::{GasCostSummary, TransactionEffects, TransactionEvents};
use super::object::Object;
use super::signature::{AuthorityQuorumSignInfo, GenericSignature};
use super::transaction::Transaction;
use crate::checkpoint as view;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointCommitment {
    #[serde(rename = "ECMHLiveObjectSetDigest")]
    EcmhLiveObjectSetDigest(EcmhLiveObjectSetDigest),
    CheckpointArtifactsDigest(CheckpointArtifactsDigest),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EndOfEpochData {
    #[serde(rename = "nextEpochCommittee")]
    pub next_epoch_committee: Vec<(AuthorityPublicKeyBytes, u64)>,
    #[serde(rename = "nextEpochProtocolVersion")]
    pub next_epoch_protocol_version: ProtocolVersion,
    #[serde(rename = "epochCommitments")]
    pub epoch_commitments: Vec<CheckpointCommitment>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointSummary {
    pub epoch: u64,
    pub sequence_number: u64,
    pub network_total_transactions: u64,
    pub content_digest: CheckpointContentsDigest,
    pub previous_digest: Option<CheckpointDigest>,
    pub epoch_rolling_gas_cost_summary: GasCostSummary,
    pub timestamp_ms: u64,
    pub checkpoint_commitments: Vec<CheckpointCommitment>,
    pub end_of_epoch_data: Option<EndOfEpochData>,
    pub version_specific_data: Vec<u8>,
}

/// `Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(
    rename = "sui_types::message_envelope::Envelope<sui_types::messages_checkpoint::CheckpointSummary, sui_types::crypto::AuthorityQuorumSignInfo<true>>"
)]
pub struct CertifiedCheckpointSummary {
    pub data: CheckpointSummary,
    pub auth_signature: AuthorityQuorumSignInfo,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionDigests {
    pub transaction: TransactionDigest,
    pub effects: TransactionEffectsDigest,
}

/// `user_signatures` is not checked to be as long as `transactions`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointContentsV1 {
    pub transactions: Vec<ExecutionDigests>,
    pub user_signatures: Vec<Vec<GenericSignature>>,
}

/// Each signature comes with the version of the address alias object it was
/// checked against, if any.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointTransactionContents {
    pub digest: ExecutionDigests,
    pub user_signatures: Vec<(GenericSignature, Option<SequenceNumber>)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointContentsV2 {
    pub transactions: Vec<CheckpointTransactionContents>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CheckpointContents {
    V1(CheckpointContentsV1),
    V2(CheckpointContentsV2),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExecutionData {
    pub transaction: Transaction,
    pub effects: TransactionEffects,
}

/// `user_signatures` is not checked to be as long as `transactions`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FullCheckpointContents {
    pub transactions: Vec<ExecutionData>,
    pub user_signatures: Vec<Vec<GenericSignature>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointTransaction {
    pub transaction: Transaction,
    pub effects: TransactionEffects,
    pub events: Option<TransactionEvents>,
    pub input_objects: Vec<Object>,
    pub output_objects: Vec<Object>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CheckpointData {
    pub checkpoint_summary: CertifiedCheckpointSummary,
    pub checkpoint_contents: CheckpointContents,
    pub transactions: Vec<CheckpointTransaction>,
}

impl From<&view::CheckpointCommitment> for CheckpointCommitment {
    fn from(v: &view::CheckpointCommitment) -> Self {
        match v.kind() {
            view::CheckpointCommitmentKind::EcmhLiveObjectSetDigest => {
                CheckpointCommitment::EcmhLiveObjectSetDigest(EcmhLiveObjectSetDigest {
                    digest: Digest::from(&v.digest),
                })
            }
            view::CheckpointCommitmentKind::CheckpointArtifactsDigest => {
                CheckpointCommitment::CheckpointArtifactsDigest(CheckpointArtifactsDigest::from(
                    &v.digest,
                ))
            }
        }
    }
}

impl From<&view::CommitteeMember> for (AuthorityPublicKeyBytes, u64) {
    fn from(v: &view::CommitteeMember) -> Self {
        (AuthorityPublicKeyBytes::from(&v.authority), v.stake.get())
    }
}

impl From<&view::EndOfEpochData<'_>> for EndOfEpochData {
    fn from(v: &view::EndOfEpochData<'_>) -> Self {
        EndOfEpochData {
            next_epoch_committee: v.next_epoch_committee.iter().map(Into::into).collect(),
            next_epoch_protocol_version: ProtocolVersion(v.next_epoch_protocol_version),
            epoch_commitments: v
                .epoch_commitments
                .iter()
                .map(CheckpointCommitment::from)
                .collect(),
        }
    }
}

impl From<&view::CheckpointSummary<'_>> for CheckpointSummary {
    fn from(v: &view::CheckpointSummary<'_>) -> Self {
        CheckpointSummary {
            epoch: v.epoch,
            sequence_number: v.sequence_number,
            network_total_transactions: v.network_total_transactions,
            content_digest: CheckpointContentsDigest::from(v.content_digest),
            previous_digest: v.previous_digest.map(CheckpointDigest::from),
            epoch_rolling_gas_cost_summary: GasCostSummary::from(&v.epoch_rolling_gas_cost_summary),
            timestamp_ms: v.timestamp_ms,
            checkpoint_commitments: v
                .checkpoint_commitments
                .iter()
                .map(CheckpointCommitment::from)
                .collect(),
            end_of_epoch_data: v.end_of_epoch_data.as_ref().map(EndOfEpochData::from),
            version_specific_data: v.version_specific_data.to_vec(),
        }
    }
}

impl From<&view::CertifiedCheckpointSummary<'_>> for CertifiedCheckpointSummary {
    fn from(v: &view::CertifiedCheckpointSummary<'_>) -> Self {
        CertifiedCheckpointSummary {
            data: CheckpointSummary::from(&v.data),
            auth_signature: AuthorityQuorumSignInfo::from(&v.auth_signature),
        }
    }
}

impl From<&view::ExecutionDigests> for ExecutionDigests {
    fn from(v: &view::ExecutionDigests) -> Self {
        ExecutionDigests {
            transaction: TransactionDigest::from(&v.transaction),
            effects: TransactionEffectsDigest::from(&v.effects),
        }
    }
}

fn signature_lists(
    lists: &[&[crate::transaction::GenericSignature<'_>]],
) -> Vec<Vec<GenericSignature>> {
    lists
        .iter()
        .map(|list| list.iter().map(GenericSignature::from).collect())
        .collect()
}

impl From<&view::CheckpointTransactionContents<'_>> for CheckpointTransactionContents {
    fn from(v: &view::CheckpointTransactionContents<'_>) -> Self {
        CheckpointTransactionContents {
            digest: ExecutionDigests::from(v.digest),
            user_signatures: v
                .user_signatures
                .iter()
                .map(|(signature, alias_version)| {
                    (
                        GenericSignature::from(signature),
                        alias_version.map(SequenceNumber),
                    )
                })
                .collect(),
        }
    }
}

impl From<&view::CheckpointContents<'_>> for CheckpointContents {
    fn from(v: &view::CheckpointContents<'_>) -> Self {
        match &v.version {
            view::VersionedCheckpointContents::V1 {
                transactions,
                user_signatures,
            } => CheckpointContents::V1(CheckpointContentsV1 {
                transactions: transactions.iter().map(ExecutionDigests::from).collect(),
                user_signatures: signature_lists(user_signatures),
            }),
            view::VersionedCheckpointContents::V2(transactions) => {
                CheckpointContents::V2(CheckpointContentsV2 {
                    transactions: transactions
                        .iter()
                        .map(CheckpointTransactionContents::from)
                        .collect(),
                })
            }
        }
    }
}

impl From<&view::ExecutionData<'_>> for ExecutionData {
    fn from(v: &view::ExecutionData<'_>) -> Self {
        ExecutionData {
            transaction: Transaction::from(&v.transaction),
            effects: TransactionEffects::from(&v.effects),
        }
    }
}

impl From<&view::FullCheckpointContents<'_>> for FullCheckpointContents {
    fn from(v: &view::FullCheckpointContents<'_>) -> Self {
        FullCheckpointContents {
            transactions: v.transactions.iter().map(ExecutionData::from).collect(),
            user_signatures: signature_lists(v.user_signatures),
        }
    }
}

impl From<&view::CheckpointTransaction<'_>> for CheckpointTransaction {
    fn from(v: &view::CheckpointTransaction<'_>) -> Self {
        CheckpointTransaction {
            transaction: Transaction::from(&v.transaction),
            effects: TransactionEffects::from(&v.effects),
            events: v.events.as_ref().map(TransactionEvents::from),
            input_objects: v.input_objects.iter().map(Object::from).collect(),
            output_objects: v.output_objects.iter().map(Object::from).collect(),
        }
    }
}

impl From<&view::CheckpointData<'_>> for CheckpointData {
    fn from(v: &view::CheckpointData<'_>) -> Self {
        CheckpointData {
            checkpoint_summary: CertifiedCheckpointSummary::from(&v.checkpoint_summary),
            checkpoint_contents: CheckpointContents::from(&v.checkpoint_contents),
            transactions: v
                .transactions
                .iter()
                .map(CheckpointTransaction::from)
                .collect(),
        }
    }
}
