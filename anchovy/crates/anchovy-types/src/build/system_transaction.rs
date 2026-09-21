// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::base::{
    AdditionalConsensusStateDigest, AuthorityPublicKeyBytes, ChainIdentifier,
    ConsensusCommitDigest, ObjectId, ProtocolVersion, RandomnessRound, SequenceNumber,
    TransactionDigest,
};
use super::object::GenesisObject;
use super::type_tag::TypeInput;
use crate::system_transaction as view;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GenesisTransaction {
    pub objects: Vec<GenesisObject>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ChangeEpoch {
    pub epoch: u64,
    pub protocol_version: ProtocolVersion,
    pub storage_charge: u64,
    pub computation_charge: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
    pub epoch_start_timestamp_ms: u64,
    /// Version, modules, dependencies.
    pub system_packages: Vec<(SequenceNumber, Vec<Vec<u8>>, Vec<ObjectId>)>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsensusCommitPrologue {
    pub epoch: u64,
    pub round: u64,
    pub commit_timestamp_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsensusCommitPrologueV2 {
    pub epoch: u64,
    pub round: u64,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: ConsensusCommitDigest,
}

/// An object id and the start version of its consensus stream.
pub type ConsensusObjectSequenceKey = (ObjectId, SequenceNumber);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ConsensusDeterminedVersionAssignments {
    CancelledTransactions(Vec<(TransactionDigest, Vec<(ObjectId, SequenceNumber)>)>),
    CancelledTransactionsV2(
        Vec<(
            TransactionDigest,
            Vec<(ConsensusObjectSequenceKey, SequenceNumber)>,
        )>,
    ),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsensusCommitPrologueV3 {
    pub epoch: u64,
    pub round: u64,
    pub sub_dag_index: Option<u64>,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: ConsensusCommitDigest,
    pub consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsensusCommitPrologueV4 {
    pub epoch: u64,
    pub round: u64,
    pub sub_dag_index: Option<u64>,
    pub commit_timestamp_ms: u64,
    pub consensus_commit_digest: ConsensusCommitDigest,
    pub consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments,
    pub additional_state_digest: AdditionalConsensusStateDigest,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct JwkId {
    pub iss: String,
    pub kid: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename = "JWK")]
pub struct Jwk {
    pub kty: String,
    pub e: String,
    pub n: String,
    pub alg: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ActiveJwk {
    pub jwk_id: JwkId,
    pub jwk: Jwk,
    pub epoch: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatorStateUpdate {
    pub epoch: u64,
    pub round: u64,
    pub new_active_jwks: Vec<ActiveJwk>,
    pub authenticator_obj_initial_shared_version: SequenceNumber,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticatorStateExpire {
    pub min_epoch: u64,
    pub authenticator_obj_initial_shared_version: SequenceNumber,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RandomnessStateUpdate {
    pub epoch: u64,
    pub randomness_round: RandomnessRound,
    pub random_bytes: Vec<u8>,
    pub randomness_obj_initial_shared_version: SequenceNumber,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ExecutionTimeObservationKey {
    MoveEntryPoint {
        package: ObjectId,
        module: String,
        function: String,
        type_arguments: Vec<TypeInput>,
    },
    TransferObjects,
    SplitCoins,
    MergeCoins,
    Publish,
    MakeMoveVec,
    Upgrade,
}

/// `std::time::Duration` as serde writes it; `nanos` is not range-checked.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Duration {
    pub secs: u64,
    pub nanos: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StoredExecutionTimeObservations {
    V1(
        Vec<(
            ExecutionTimeObservationKey,
            Vec<(AuthorityPublicKeyBytes, Duration)>,
        )>,
    ),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteAccumulatorStorageCost {
    pub storage_cost: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum EndOfEpochTransactionKind {
    ChangeEpoch(ChangeEpoch),
    AuthenticatorStateCreate,
    AuthenticatorStateExpire(AuthenticatorStateExpire),
    RandomnessStateCreate,
    DenyListStateCreate,
    BridgeStateCreate(ChainIdentifier),
    BridgeCommitteeInit(SequenceNumber),
    StoreExecutionTimeObservations(StoredExecutionTimeObservations),
    AccumulatorRootCreate,
    CoinRegistryCreate,
    DisplayRegistryCreate,
    AddressAliasStateCreate,
    WriteAccumulatorStorageCost(WriteAccumulatorStorageCost),
    ForwardingAddressRegistryCreate,
}

impl From<&view::SystemPackage<'_>> for (SequenceNumber, Vec<Vec<u8>>, Vec<ObjectId>) {
    fn from(v: &view::SystemPackage<'_>) -> Self {
        (
            SequenceNumber(v.version),
            v.modules.iter().map(|module| module.to_vec()).collect(),
            v.dependencies.iter().map(ObjectId::from).collect(),
        )
    }
}

impl From<&view::ChangeEpoch<'_>> for ChangeEpoch {
    fn from(v: &view::ChangeEpoch<'_>) -> Self {
        ChangeEpoch {
            epoch: v.epoch,
            protocol_version: ProtocolVersion(v.protocol_version),
            storage_charge: v.storage_charge,
            computation_charge: v.computation_charge,
            storage_rebate: v.storage_rebate,
            non_refundable_storage_fee: v.non_refundable_storage_fee,
            epoch_start_timestamp_ms: v.epoch_start_timestamp_ms,
            system_packages: v.system_packages.iter().map(Into::into).collect(),
        }
    }
}

impl From<&view::ConsensusCommitPrologue> for ConsensusCommitPrologue {
    fn from(v: &view::ConsensusCommitPrologue) -> Self {
        ConsensusCommitPrologue {
            epoch: v.epoch,
            round: v.round,
            commit_timestamp_ms: v.commit_timestamp_ms,
        }
    }
}

impl From<&view::ConsensusCommitPrologueV2<'_>> for ConsensusCommitPrologueV2 {
    fn from(v: &view::ConsensusCommitPrologueV2<'_>) -> Self {
        ConsensusCommitPrologueV2 {
            epoch: v.epoch,
            round: v.round,
            commit_timestamp_ms: v.commit_timestamp_ms,
            consensus_commit_digest: ConsensusCommitDigest::from(v.consensus_commit_digest),
        }
    }
}

impl From<&view::ConsensusObjectVersion> for (ConsensusObjectSequenceKey, SequenceNumber) {
    fn from(v: &view::ConsensusObjectVersion) -> Self {
        (
            (ObjectId::from(&v.id), SequenceNumber(v.start_version.get())),
            SequenceNumber(v.version.get()),
        )
    }
}

impl From<&view::ConsensusDeterminedVersionAssignments<'_>>
    for ConsensusDeterminedVersionAssignments
{
    fn from(v: &view::ConsensusDeterminedVersionAssignments<'_>) -> Self {
        match v {
            view::ConsensusDeterminedVersionAssignments::CancelledTransactions(transactions) => {
                ConsensusDeterminedVersionAssignments::CancelledTransactions(
                    transactions
                        .iter()
                        .map(|(digest, objects)| {
                            (
                                TransactionDigest::from(*digest),
                                objects.iter().map(Into::into).collect(),
                            )
                        })
                        .collect(),
                )
            }
            view::ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(transactions) => {
                ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(
                    transactions
                        .iter()
                        .map(|(digest, objects)| {
                            (
                                TransactionDigest::from(*digest),
                                objects.iter().map(Into::into).collect(),
                            )
                        })
                        .collect(),
                )
            }
        }
    }
}

impl From<&view::ConsensusCommitPrologueV3<'_>> for ConsensusCommitPrologueV3 {
    fn from(v: &view::ConsensusCommitPrologueV3<'_>) -> Self {
        ConsensusCommitPrologueV3 {
            epoch: v.epoch,
            round: v.round,
            sub_dag_index: v.sub_dag_index,
            commit_timestamp_ms: v.commit_timestamp_ms,
            consensus_commit_digest: ConsensusCommitDigest::from(v.consensus_commit_digest),
            consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments::from(
                &v.consensus_determined_version_assignments,
            ),
        }
    }
}

impl From<&view::ConsensusCommitPrologueV4<'_>> for ConsensusCommitPrologueV4 {
    fn from(v: &view::ConsensusCommitPrologueV4<'_>) -> Self {
        ConsensusCommitPrologueV4 {
            epoch: v.epoch,
            round: v.round,
            sub_dag_index: v.sub_dag_index,
            commit_timestamp_ms: v.commit_timestamp_ms,
            consensus_commit_digest: ConsensusCommitDigest::from(v.consensus_commit_digest),
            consensus_determined_version_assignments: ConsensusDeterminedVersionAssignments::from(
                &v.consensus_determined_version_assignments,
            ),
            additional_state_digest: AdditionalConsensusStateDigest::from(
                v.additional_state_digest,
            ),
        }
    }
}

impl From<&view::JwkId<'_>> for JwkId {
    fn from(v: &view::JwkId<'_>) -> Self {
        JwkId {
            iss: v.iss.to_owned(),
            kid: v.kid.to_owned(),
        }
    }
}

impl From<&view::Jwk<'_>> for Jwk {
    fn from(v: &view::Jwk<'_>) -> Self {
        Jwk {
            kty: v.kty.to_owned(),
            e: v.e.to_owned(),
            n: v.n.to_owned(),
            alg: v.alg.to_owned(),
        }
    }
}

impl From<&view::ActiveJwk<'_>> for ActiveJwk {
    fn from(v: &view::ActiveJwk<'_>) -> Self {
        ActiveJwk {
            jwk_id: JwkId::from(&v.jwk_id),
            jwk: Jwk::from(&v.jwk),
            epoch: v.epoch,
        }
    }
}

impl From<&view::AuthenticatorStateUpdate<'_>> for AuthenticatorStateUpdate {
    fn from(v: &view::AuthenticatorStateUpdate<'_>) -> Self {
        AuthenticatorStateUpdate {
            epoch: v.epoch,
            round: v.round,
            new_active_jwks: v.new_active_jwks.iter().map(ActiveJwk::from).collect(),
            authenticator_obj_initial_shared_version: SequenceNumber(
                v.authenticator_obj_initial_shared_version,
            ),
        }
    }
}

impl From<&view::RandomnessStateUpdate<'_>> for RandomnessStateUpdate {
    fn from(v: &view::RandomnessStateUpdate<'_>) -> Self {
        RandomnessStateUpdate {
            epoch: v.epoch,
            randomness_round: RandomnessRound(v.randomness_round),
            random_bytes: v.random_bytes.to_vec(),
            randomness_obj_initial_shared_version: SequenceNumber(
                v.randomness_obj_initial_shared_version,
            ),
        }
    }
}

impl From<&view::ExecutionTimeObservationKey<'_>> for ExecutionTimeObservationKey {
    fn from(v: &view::ExecutionTimeObservationKey<'_>) -> Self {
        use ExecutionTimeObservationKey as B;
        use view::ExecutionTimeObservationKey as V;
        match v {
            V::MoveEntryPoint {
                package,
                module,
                function,
                type_arguments,
            } => B::MoveEntryPoint {
                package: ObjectId::from(*package),
                module: (*module).to_owned(),
                function: (*function).to_owned(),
                type_arguments: type_arguments.iter().map(TypeInput::from).collect(),
            },
            V::TransferObjects => B::TransferObjects,
            V::SplitCoins => B::SplitCoins,
            V::MergeCoins => B::MergeCoins,
            V::Publish => B::Publish,
            V::MakeMoveVec => B::MakeMoveVec,
            V::Upgrade => B::Upgrade,
        }
    }
}

impl From<&view::AuthorityObservation> for (AuthorityPublicKeyBytes, Duration) {
    fn from(v: &view::AuthorityObservation) -> Self {
        (
            AuthorityPublicKeyBytes::from(&v.authority),
            Duration {
                secs: v.secs.get(),
                nanos: v.nanos.get(),
            },
        )
    }
}

impl From<&view::StoredExecutionTimeObservations<'_>> for StoredExecutionTimeObservations {
    fn from(v: &view::StoredExecutionTimeObservations<'_>) -> Self {
        StoredExecutionTimeObservations::V1(
            v.0.iter()
                .map(|(key, observations)| {
                    (
                        ExecutionTimeObservationKey::from(key),
                        observations.iter().map(Into::into).collect(),
                    )
                })
                .collect(),
        )
    }
}

impl From<&view::EndOfEpochTransactionKind<'_>> for EndOfEpochTransactionKind {
    fn from(v: &view::EndOfEpochTransactionKind<'_>) -> Self {
        use EndOfEpochTransactionKind as B;
        use view::EndOfEpochTransactionKind as V;
        match v {
            V::ChangeEpoch(change) => B::ChangeEpoch(ChangeEpoch::from(&**change)),
            V::AuthenticatorStateCreate => B::AuthenticatorStateCreate,
            V::AuthenticatorStateExpire {
                min_epoch,
                authenticator_obj_initial_shared_version,
            } => B::AuthenticatorStateExpire(AuthenticatorStateExpire {
                min_epoch: *min_epoch,
                authenticator_obj_initial_shared_version: SequenceNumber(
                    *authenticator_obj_initial_shared_version,
                ),
            }),
            V::RandomnessStateCreate => B::RandomnessStateCreate,
            V::DenyListStateCreate => B::DenyListStateCreate,
            V::BridgeStateCreate(chain) => B::BridgeStateCreate(ChainIdentifier::from(*chain)),
            V::BridgeCommitteeInit(version) => B::BridgeCommitteeInit(SequenceNumber(*version)),
            V::StoreExecutionTimeObservations(observations) => B::StoreExecutionTimeObservations(
                StoredExecutionTimeObservations::from(observations),
            ),
            V::AccumulatorRootCreate => B::AccumulatorRootCreate,
            V::CoinRegistryCreate => B::CoinRegistryCreate,
            V::DisplayRegistryCreate => B::DisplayRegistryCreate,
            V::AddressAliasStateCreate => B::AddressAliasStateCreate,
            V::WriteAccumulatorStorageCost { storage_cost } => {
                B::WriteAccumulatorStorageCost(WriteAccumulatorStorageCost {
                    storage_cost: *storage_cost,
                })
            }
            V::ForwardingAddressRegistryCreate => B::ForwardingAddressRegistryCreate,
        }
    }
}
