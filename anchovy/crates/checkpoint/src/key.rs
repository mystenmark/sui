// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This validator's protocol key: the BLS12-381 (min-sig) key it signs
//! checkpoint summaries with, as the reference's `AuthorityKeyPair`.

use fastcrypto::bls12381::min_sig::{BLS12381KeyPair, BLS12381PrivateKey};
use fastcrypto::error::FastCryptoError;
use fastcrypto::traits::{KeyPair, Signer, ToFromBytes};

pub struct ProtocolKey {
    keypair: BLS12381KeyPair,
    public: [u8; 96],
}

impl ProtocolKey {
    pub fn new(keypair: BLS12381KeyPair) -> ProtocolKey {
        let public = keypair
            .public()
            .as_bytes()
            .try_into()
            .expect("a min-sig public key is 96 bytes");
        ProtocolKey { keypair, public }
    }

    /// The key from its 32-byte private scalar.
    pub fn from_private_bytes(bytes: &[u8]) -> Result<ProtocolKey, FastCryptoError> {
        Ok(ProtocolKey::new(BLS12381KeyPair::from(
            BLS12381PrivateKey::from_bytes(bytes)?,
        )))
    }

    /// The `AuthorityPublicKeyBytes` this key signs as.
    pub fn public(&self) -> &[u8; 96] {
        &self.public
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 48] {
        self.keypair
            .sign(message)
            .as_bytes()
            .try_into()
            .expect("a min-sig signature is 48 bytes")
    }
}

impl std::fmt::Debug for ProtocolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolKey").finish_non_exhaustive()
    }
}
