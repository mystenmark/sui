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

/// sui's format snapshot omits `Passkey`, which it never samples.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CompressedSignature {
    Ed25519(#[serde(with = "super::byte_array")] [u8; 64]),
    Secp256k1(#[serde(with = "super::byte_array")] [u8; 64]),
    Secp256r1(#[serde(with = "super::byte_array")] [u8; 64]),
    ZkLogin(ZkLoginAuthenticatorAsBytes),
    Passkey(#[serde(with = "serde_bytes")] Vec<u8>),
    MLDSA65(#[serde(with = "super::byte_array")] [u8; 3309]),
}

/// sui's format snapshot omits `Passkey`, which it never samples.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum PublicKey {
    Ed25519([u8; 32]),
    Secp256k1(#[serde(with = "super::byte_array")] [u8; 33]),
    Secp256r1(#[serde(with = "super::byte_array")] [u8; 33]),
    ZkLogin(ZkLoginPublicIdentifier),
    Passkey(#[serde(with = "super::byte_array")] [u8; 33]),
    MLDSA65(#[serde(with = "super::byte_array")] [u8; 1952]),
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

impl From<&view::CompressedSignature<'_>> for CompressedSignature {
    fn from(v: &view::CompressedSignature<'_>) -> Self {
        match v {
            view::CompressedSignature::Ed25519(bytes) => CompressedSignature::Ed25519(**bytes),
            view::CompressedSignature::Secp256k1(bytes) => CompressedSignature::Secp256k1(**bytes),
            view::CompressedSignature::Secp256r1(bytes) => CompressedSignature::Secp256r1(**bytes),
            view::CompressedSignature::ZkLogin(bytes) => {
                CompressedSignature::ZkLogin(ZkLoginAuthenticatorAsBytes(bytes.to_vec()))
            }
            view::CompressedSignature::Passkey(bytes) => {
                CompressedSignature::Passkey(bytes.to_vec())
            }
            view::CompressedSignature::MLDSA65(bytes) => CompressedSignature::MLDSA65(**bytes),
        }
    }
}

impl From<&view::PublicKey<'_>> for PublicKey {
    fn from(v: &view::PublicKey<'_>) -> Self {
        match v {
            view::PublicKey::Ed25519(bytes) => PublicKey::Ed25519(**bytes),
            view::PublicKey::Secp256k1(bytes) => PublicKey::Secp256k1(**bytes),
            view::PublicKey::Secp256r1(bytes) => PublicKey::Secp256r1(**bytes),
            view::PublicKey::ZkLogin(bytes) => {
                PublicKey::ZkLogin(ZkLoginPublicIdentifier(bytes.to_vec()))
            }
            view::PublicKey::Passkey(bytes) => PublicKey::Passkey(**bytes),
            view::PublicKey::MLDSA65(bytes) => PublicKey::MLDSA65(**bytes),
        }
    }
}

impl From<&view::MultiSigPublicKey<'_>> for MultiSigPublicKey {
    fn from(v: &view::MultiSigPublicKey<'_>) -> Self {
        MultiSigPublicKey {
            pk_map: v
                .pk_map
                .iter()
                .map(|(key, weight)| (PublicKey::from(key), *weight))
                .collect(),
            threshold: v.threshold,
        }
    }
}

impl From<&view::MultiSig<'_>> for MultiSig {
    fn from(v: &view::MultiSig<'_>) -> Self {
        MultiSig {
            sigs: v.sigs.iter().map(CompressedSignature::from).collect(),
            bitmap: v.bitmap,
            multisig_pk: MultiSigPublicKey::from(&v.multisig_pk),
        }
    }
}
