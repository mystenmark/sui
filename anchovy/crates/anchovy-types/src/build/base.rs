// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::base as view;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountAddress(pub [u8; 32]);

impl From<&view::AccountAddress> for AccountAddress {
    fn from(v: &view::AccountAddress) -> Self {
        AccountAddress(v.0)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SuiAddress(pub [u8; 32]);

impl From<&view::SuiAddress> for SuiAddress {
    fn from(v: &view::SuiAddress) -> Self {
        SuiAddress(v.0)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename = "ObjectID")]
pub struct ObjectId(pub AccountAddress);

impl From<&view::ObjectId> for ObjectId {
    fn from(v: &view::ObjectId) -> Self {
        ObjectId(AccountAddress(v.0))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceNumber(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersion(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomnessRound(pub u64);

/// A byte string, not a tuple, and one that must be 32 bytes long.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(#[serde(with = "super::fixed_bytes")] pub [u8; 32]);

impl From<&view::Digest> for Digest {
    fn from(v: &view::Digest) -> Self {
        Digest(v.bytes)
    }
}

macro_rules! digest {
    ($name:ident) => {
        #[derive(
            Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
        )]
        pub struct $name(pub Digest);

        impl From<&view::Digest> for $name {
            fn from(v: &view::Digest) -> Self {
                $name(Digest(v.bytes))
            }
        }
    };
}

digest!(ObjectDigest);
digest!(TransactionDigest);
digest!(TransactionEffectsDigest);
digest!(TransactionEventsDigest);
digest!(EffectsAuxDataDigest);
digest!(CheckpointDigest);
digest!(CheckpointContentsDigest);
digest!(CheckpointArtifactsDigest);
digest!(ConsensusCommitDigest);
digest!(AdditionalConsensusStateDigest);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainIdentifier(pub CheckpointDigest);

impl From<&view::Digest> for ChainIdentifier {
    fn from(v: &view::Digest) -> Self {
        ChainIdentifier(CheckpointDigest(Digest(v.bytes)))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename = "ECMHLiveObjectSetDigest")]
pub struct EcmhLiveObjectSetDigest {
    pub digest: Digest,
}

/// A byte string that must be 96 bytes long; not checked to be a BLS point.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorityPublicKeyBytes(#[serde(with = "super::fixed_bytes")] pub [u8; 96]);

impl From<&view::AuthorityName> for AuthorityPublicKeyBytes {
    fn from(v: &view::AuthorityName) -> Self {
        AuthorityPublicKeyBytes(v.bytes)
    }
}

pub type ObjectRef = (ObjectId, SequenceNumber, ObjectDigest);

impl From<&view::ObjectRef> for ObjectRef {
    fn from(v: &view::ObjectRef) -> Self {
        (
            ObjectId::from(&v.id),
            SequenceNumber(v.version.get()),
            ObjectDigest::from(&v.digest),
        )
    }
}

impl From<&view::ObjectKey> for (ObjectId, SequenceNumber) {
    fn from(v: &view::ObjectKey) -> Self {
        (ObjectId::from(&v.id), SequenceNumber(v.version.get()))
    }
}
