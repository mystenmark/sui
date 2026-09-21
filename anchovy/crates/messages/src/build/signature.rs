// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::checkpoint as checkpoint_view;
use crate::signature as view;
use crate::transaction as transaction_view;

/// A flag byte and a scheme-specific encoding, as opaque bytes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GenericSignature(pub Vec<u8>);

impl From<&transaction_view::GenericSignature<'_>> for GenericSignature {
    fn from(v: &transaction_view::GenericSignature<'_>) -> Self {
        GenericSignature(v.0.to_vec())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ZkLoginAuthenticatorAsBytes(pub Vec<u8>);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ZkLoginPublicIdentifier(pub Vec<u8>);

/// Known gap: the reference has a `Passkey` variant at index 4 that the
/// format snapshot omits, because the snapshot only samples this type.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CompressedSignature {
    Ed25519(#[serde(with = "super::byte_array")] [u8; 64]),
    Secp256k1(#[serde(with = "super::byte_array")] [u8; 64]),
    Secp256r1(#[serde(with = "super::byte_array")] [u8; 64]),
    ZkLogin(ZkLoginAuthenticatorAsBytes),
}

/// Known gap: the reference has a `Passkey` variant at index 4 that the
/// format snapshot omits, because the snapshot only samples this type.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PublicKey {
    Ed25519([u8; 32]),
    Secp256k1(#[serde(with = "super::byte_array")] [u8; 33]),
    Secp256r1(#[serde(with = "super::byte_array")] [u8; 33]),
    ZkLogin(ZkLoginPublicIdentifier),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MultiSigPublicKey {
    pub pk_map: Vec<(PublicKey, u8)>,
    pub threshold: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MultiSig {
    pub sigs: Vec<CompressedSignature>,
    pub bitmap: u16,
    pub multisig_pk: MultiSigPublicKey,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EmptySignInfo {}

/// An aggregate BLS signature and a serialized roaring bitmap of signers,
/// both opaque.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthorityQuorumSignInfo {
    pub epoch: u64,
    #[serde(with = "super::byte_array")]
    pub signature: [u8; 48],
    #[serde(with = "serde_bytes")]
    pub signers_map: Vec<u8>,
}

impl From<&checkpoint_view::AuthorityQuorumSignInfo<'_>> for AuthorityQuorumSignInfo {
    fn from(v: &checkpoint_view::AuthorityQuorumSignInfo<'_>) -> Self {
        AuthorityQuorumSignInfo {
            epoch: v.epoch,
            signature: *v.signature,
            signers_map: v.signers_map.to_vec(),
        }
    }
}

/// A view held a `Passkey` variant, which the mirrors cannot hold until the
/// snapshot has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasskeyUnsupported;

impl TryFrom<&view::CompressedSignature<'_>> for CompressedSignature {
    type Error = PasskeyUnsupported;

    fn try_from(v: &view::CompressedSignature<'_>) -> Result<Self, PasskeyUnsupported> {
        match v {
            view::CompressedSignature::Ed25519(bytes) => Ok(CompressedSignature::Ed25519(**bytes)),
            view::CompressedSignature::Secp256k1(bytes) => {
                Ok(CompressedSignature::Secp256k1(**bytes))
            }
            view::CompressedSignature::Secp256r1(bytes) => {
                Ok(CompressedSignature::Secp256r1(**bytes))
            }
            view::CompressedSignature::ZkLogin(bytes) => Ok(CompressedSignature::ZkLogin(
                ZkLoginAuthenticatorAsBytes(bytes.to_vec()),
            )),
            view::CompressedSignature::Passkey(_) => Err(PasskeyUnsupported),
        }
    }
}

impl TryFrom<&view::PublicKey<'_>> for PublicKey {
    type Error = PasskeyUnsupported;

    fn try_from(v: &view::PublicKey<'_>) -> Result<Self, PasskeyUnsupported> {
        match v {
            view::PublicKey::Ed25519(bytes) => Ok(PublicKey::Ed25519(**bytes)),
            view::PublicKey::Secp256k1(bytes) => Ok(PublicKey::Secp256k1(**bytes)),
            view::PublicKey::Secp256r1(bytes) => Ok(PublicKey::Secp256r1(**bytes)),
            view::PublicKey::ZkLogin(bytes) => {
                Ok(PublicKey::ZkLogin(ZkLoginPublicIdentifier(bytes.to_vec())))
            }
            view::PublicKey::Passkey(_) => Err(PasskeyUnsupported),
        }
    }
}

impl TryFrom<&view::MultiSigPublicKey<'_>> for MultiSigPublicKey {
    type Error = PasskeyUnsupported;

    fn try_from(v: &view::MultiSigPublicKey<'_>) -> Result<Self, PasskeyUnsupported> {
        let mut pk_map = Vec::with_capacity(v.pk_map.len());
        for (key, weight) in v.pk_map {
            pk_map.push((PublicKey::try_from(key)?, *weight));
        }
        Ok(MultiSigPublicKey {
            pk_map,
            threshold: v.threshold,
        })
    }
}

impl TryFrom<&view::MultiSig<'_>> for MultiSig {
    type Error = PasskeyUnsupported;

    fn try_from(v: &view::MultiSig<'_>) -> Result<Self, PasskeyUnsupported> {
        let mut sigs = Vec::with_capacity(v.sigs.len());
        for sig in v.sigs {
            sigs.push(CompressedSignature::try_from(sig)?);
        }
        Ok(MultiSig {
            sigs,
            bitmap: v.bitmap,
            multisig_pk: MultiSigPublicKey::try_from(&v.multisig_pk)?,
        })
    }
}
