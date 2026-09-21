// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The structure inside a multisig `GenericSignature`. Message parsing keeps
//! signatures as opaque bytes; validation parses them with these when it
//! needs to. Nothing here checks keys, weights, thresholds or bitmaps.

use crate::arena::Alloc;
use crate::error::{ParseError, Result};
use crate::reader::Reader;

/// The first byte of a `GenericSignature`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignatureScheme {
    Ed25519 = 0,
    Secp256k1 = 1,
    Secp256r1 = 2,
    MultiSig = 3,
    Bls12381 = 4,
    ZkLogin = 5,
    Passkey = 6,
}

impl SignatureScheme {
    pub fn from_flag(flag: u8) -> Option<SignatureScheme> {
        Some(match flag {
            0 => SignatureScheme::Ed25519,
            1 => SignatureScheme::Secp256k1,
            2 => SignatureScheme::Secp256r1,
            3 => SignatureScheme::MultiSig,
            4 => SignatureScheme::Bls12381,
            5 => SignatureScheme::ZkLogin,
            6 => SignatureScheme::Passkey,
            _ => return None,
        })
    }
}

/// The checked-in format snapshot lacks `Passkey` here and in `PublicKey`:
/// the reference traces these enums from sample values that have none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompressedSignature<'a> {
    Ed25519(&'a [u8; 64]),
    Secp256k1(&'a [u8; 64]),
    Secp256r1(&'a [u8; 64]),
    ZkLogin(&'a [u8]),
    Passkey(&'a [u8]),
}

impl<'a> CompressedSignature<'a> {
    /// `ZkLogin` of no bytes.
    pub const MIN_WIRE_SIZE: usize = 2;

    pub fn parse(r: &mut Reader<'a>) -> Result<CompressedSignature<'a>> {
        Ok(match r.variant()? {
            0 => CompressedSignature::Ed25519(r.array()?),
            1 => CompressedSignature::Secp256k1(r.array()?),
            2 => CompressedSignature::Secp256r1(r.array()?),
            3 => CompressedSignature::ZkLogin(r.byte_vec()?),
            4 => CompressedSignature::Passkey(r.byte_vec()?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "CompressedSignature",
                    tag,
                });
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PublicKey<'a> {
    Ed25519(&'a [u8; 32]),
    Secp256k1(&'a [u8; 33]),
    Secp256r1(&'a [u8; 33]),
    ZkLogin(&'a [u8]),
    Passkey(&'a [u8; 33]),
}

impl<'a> PublicKey<'a> {
    pub fn parse(r: &mut Reader<'a>) -> Result<PublicKey<'a>> {
        Ok(match r.variant()? {
            0 => PublicKey::Ed25519(r.array()?),
            1 => PublicKey::Secp256k1(r.array()?),
            2 => PublicKey::Secp256r1(r.array()?),
            3 => PublicKey::ZkLogin(r.byte_vec()?),
            4 => PublicKey::Passkey(r.array()?),
            tag => {
                return Err(ParseError::UnknownVariant {
                    ty: "PublicKey",
                    tag,
                });
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MultiSigPublicKey<'a> {
    /// Each key with its weight.
    pub pk_map: &'a [(PublicKey<'a>, u8)],
    pub threshold: u16,
}

impl<'a> MultiSigPublicKey<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<MultiSigPublicKey<'a>> {
        // A `ZkLogin` key of no bytes, and a weight.
        let n = r.seq_len(2 + 1)?;
        let mut pk_map = a.slice(n)?;
        for _ in 0..n {
            pk_map.push((PublicKey::parse(r)?, r.u8()?));
        }
        Ok(MultiSigPublicKey {
            pk_map: pk_map.finish(),
            threshold: r.u16()?,
        })
    }
}

/// What follows the scheme flag of a multisig `GenericSignature`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MultiSig<'a> {
    pub sigs: &'a [CompressedSignature<'a>],
    /// Bit `i` is set if the key at `pk_map[i]` signed.
    pub bitmap: u16,
    pub multisig_pk: MultiSigPublicKey<'a>,
}

impl<'a> MultiSig<'a> {
    pub fn parse<A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<MultiSig<'a>> {
        let n = r.seq_len(CompressedSignature::MIN_WIRE_SIZE)?;
        let mut sigs = a.slice(n)?;
        for _ in 0..n {
            sigs.push(CompressedSignature::parse(r)?);
        }
        Ok(MultiSig {
            sigs: sigs.finish(),
            bitmap: r.u16()?,
            multisig_pk: MultiSigPublicKey::parse(r, a)?,
        })
    }
}

crate::impl_wire!(MultiSig);
crate::impl_wire!(MultiSigPublicKey);
