// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `GenericSignature` contents: what the reference's `Deserialize` checks,
//! which message parsing leaves to validation. A signature that fails here
//! makes the reference reject the whole transaction while deserializing.

use containers::Bump;
use fastcrypto::traits::ToFromBytes;
use messages::arena::BumpAlloc;
use messages::reader::Reader;
use messages::signature::{CompressedSignature, MultiSig, MultiSigPublicKey, PublicKey};

use crate::{Error, ErrorKind};

/// `MAX_SIGNER_IN_MULTISIG`.
const MAX_SIGNERS: usize = 10;
/// `MAX_BITMAP_VALUE`: one bit per possible signer.
const MAX_BITMAP: u16 = 0b11_1111_1111;
/// `MAX_VALIDATOR_COUNT`, which bounds a roaring bitmap's cardinality.
const MAX_ROARING_CARDINALITY: u64 = 150;

/// Flag, signature, key: the lengths of the single-key schemes.
const ED25519_SIGNATURE_LEN: usize = 1 + 64 + 32;
const SECP256_SIGNATURE_LEN: usize = 1 + 64 + 33;

#[derive(Clone, Copy, Debug)]
pub enum ParsedSignature<'a> {
    /// Ed25519, Secp256k1 or Secp256r1: `flag || signature || public key`.
    Simple(&'a [u8]),
    MultiSig(MultiSig<'a>),
    /// The pre-2023 multisig format, converted: keys decoded, the roaring
    /// bitmap reduced to the new format's `u16`.
    MultiSigLegacy {
        multisig: MultiSig<'a>,
        /// The reference re-encodes the bitmap canonically, so the size it
        /// counts can differ from the wire length.
        serialized_len: usize,
    },
    /// Checked, but kept as bytes: the parsed inputs own heap strings, so
    /// verification parses them again.
    ZkLogin(&'a [u8]),
    Passkey(Passkey<'a>),
}

/// A passkey (`WebAuthn`) assertion over a transaction: the authenticator signed
/// `authenticator_data || sha256(client_data_json)`, and the client data's
/// challenge is the transaction's signing digest.
#[derive(Clone, Copy, Debug)]
pub struct Passkey<'a> {
    pub authenticator_data: &'a [u8],
    pub client_data_json: &'a str,
    pub challenge: [u8; 32],
    /// A Secp256r1 signature, `r || s`, both in range.
    pub signature: &'a [u8; 64],
    /// A valid compressed Secp256r1 point.
    pub public_key: &'a [u8; 33],
}

impl ParsedSignature<'_> {
    /// The reference's `GenericSignature` variant.
    pub fn variant(&self) -> &'static str {
        match self {
            ParsedSignature::Simple(_) => "Signature",
            ParsedSignature::MultiSig(_) => "MultiSig",
            ParsedSignature::MultiSigLegacy { .. } => "MultiSigLegacy",
            ParsedSignature::ZkLogin(_) => "ZkLoginAuthenticator",
            ParsedSignature::Passkey(_) => "PasskeyAuthenticator",
        }
    }
}

fn malformed(what: &str) -> Error {
    Error::new(ErrorKind::TransactionDeserializationError, what)
}

/// Parses a `GenericSignature`, returning it with the length the reference
/// gives its re-serialization.
pub fn parse<'a>(bytes: &'a [u8], bump: &'a Bump) -> Result<(ParsedSignature<'a>, usize), Error> {
    let Some(&flag) = bytes.first() else {
        return Err(malformed("empty signature"));
    };
    let parsed = match flag {
        0 if bytes.len() == ED25519_SIGNATURE_LEN => ParsedSignature::Simple(bytes),
        1 | 2 if bytes.len() == SECP256_SIGNATURE_LEN => ParsedSignature::Simple(bytes),
        0..=2 => return Err(malformed("signature of the wrong length")),
        // The new format is tried first; anything it rejects is read as
        // the legacy one.
        3 => match multisig(&bytes[1..], bump) {
            Some(m) => ParsedSignature::MultiSig(m),
            None => legacy_multisig(&bytes[1..], bump)?,
        },
        5 => {
            let len =
                zklogin_serialized_len(&bytes[1..]).ok_or_else(|| malformed("invalid zklogin"))?;
            return Ok((ParsedSignature::ZkLogin(bytes), 1 + len));
        }
        6 => ParsedSignature::Passkey(
            passkey(&bytes[1..]).ok_or_else(|| malformed("invalid passkey"))?,
        ),
        _ => return Err(malformed("unknown signature scheme")),
    };
    let len = match parsed {
        ParsedSignature::MultiSigLegacy { serialized_len, .. } => serialized_len,
        _ => bytes.len(),
    };
    Ok((parsed, len))
}

/// `MultiSig::from_bytes`: the BCS body, then `init_and_validate`.
fn multisig<'a>(body: &'a [u8], bump: &'a Bump) -> Option<MultiSig<'a>> {
    let mut r = Reader::new(body);
    let m = MultiSig::parse(&mut r, &mut BumpAlloc(bump)).ok()?;
    r.finish().ok()?;
    let valid = !m.sigs.is_empty()
        && m.sigs.len() <= m.multisig_pk.pk_map.len()
        && m.bitmap <= MAX_BITMAP
        && multisig_pk_valid(&m.multisig_pk);
    valid.then_some(m)
}

/// `MultiSigPublicKey::validate`, in its order: the weight sum is taken only
/// once there are at most ten keys, so it fits a `u16`.
fn multisig_pk_valid(pk: &MultiSigPublicKey<'_>) -> bool {
    let map = pk.pk_map;
    pk.threshold != 0
        && !map.is_empty()
        && map.len() <= MAX_SIGNERS
        && map.iter().all(|(_, w)| *w != 0)
        && map.iter().map(|(_, w)| u16::from(*w)).sum::<u16>() >= pk.threshold
        && map
            .iter()
            .enumerate()
            .all(|(i, (k, _))| map[i + 1..].iter().all(|(other, _)| other != k))
}

/// A zkLogin authenticator, decoded with fastcrypto-zkp's own types so that
/// field elements and JWT details are accepted exactly as the reference
/// does.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ZkLoginAuthenticator {
    pub inputs: fastcrypto_zkp::bn254::zk_login::ZkLoginInputs,
    pub max_epoch: u64,
    /// `flag || signature || key` of the ephemeral key.
    pub user_signature: Vec<u8>,
}

/// Decodes a zkLogin body (after the flag) as the reference does: BCS,
/// the ephemeral signature's length, then `ZkLoginInputs::init`.
pub(crate) fn zklogin(body: &[u8]) -> Option<ZkLoginAuthenticator> {
    let mut zk: ZkLoginAuthenticator = bcs::from_bytes(body).ok()?;
    let sig_len_ok = match zk.user_signature.first() {
        Some(0) => zk.user_signature.len() == ED25519_SIGNATURE_LEN,
        Some(1 | 2) => zk.user_signature.len() == SECP256_SIGNATURE_LEN,
        _ => false,
    };
    if !sig_len_ok {
        return None;
    }
    zk.inputs.init().ok()?;
    Some(zk)
}

/// The length the reference re-serializes a zkLogin body to: field elements
/// are reduced and printed canonically, so it can differ from the wire.
fn zklogin_serialized_len(body: &[u8]) -> Option<usize> {
    let zk = zklogin(body)?;
    bcs::serialized_size(&zk).ok()
}

/// The fields of the client data the reference reads, with its serde
/// attributes (`passkey_types::webauthn::CollectedClientData`), so that the
/// same JSON is accepted.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CollectedClientData {
    #[serde(rename = "type")]
    ty: ClientDataType,
    challenge: String,
    origin: String,
    #[serde(default)]
    cross_origin: Option<bool>,
    #[serde(flatten)]
    extra_data: (),
    #[serde(flatten)]
    unknown_keys: serde_json::Map<String, serde_json::Value>,
}

#[derive(serde::Deserialize, PartialEq)]
enum ClientDataType {
    #[serde(rename = "webauthn.create")]
    Create,
    #[serde(rename = "webauthn.get")]
    Get,
    #[serde(rename = "payment.get")]
    PaymentGet,
}

/// `PasskeyAuthenticator`'s deserialization: authenticator data, client
/// data JSON, and a Secp256r1 signature whose key and signature fastcrypto
/// accepts.
fn passkey(body: &[u8]) -> Option<Passkey<'_>> {
    let mut r = Reader::new(body);
    let authenticator_data = r.byte_vec().ok()?;
    let client_data_json = r.str().ok()?;
    let user_signature = r.byte_vec().ok()?;
    r.finish().ok()?;

    let client_data: CollectedClientData = serde_json::from_str(client_data_json).ok()?;
    if client_data.ty != ClientDataType::Get {
        return None;
    }
    let challenge = {
        use base64ct::Encoding as _;
        let mut buf = [0u8; 32];
        let decoded = base64ct::Base64UrlUnpadded::decode(&client_data.challenge, &mut buf).ok()?;
        if decoded.len() != 32 {
            return None;
        }
        buf
    };

    // `Signature::from_bytes`, which must be Secp256r1: flag, signature, key.
    if user_signature.len() != SECP256_SIGNATURE_LEN || user_signature[0] != 2 {
        return None;
    }
    let signature: &[u8; 64] = user_signature[1..65].try_into().ok()?;
    let public_key: &[u8; 33] = user_signature[65..].try_into().ok()?;
    fastcrypto::secp256r1::Secp256r1PublicKey::from_bytes(public_key).ok()?;
    fastcrypto::secp256r1::Secp256r1Signature::from_bytes(signature).ok()?;
    Some(Passkey {
        authenticator_data,
        client_data_json,
        challenge,
        signature,
        public_key,
    })
}

/// `MultiSigLegacy::from_bytes`: the same signatures, a roaring bitmap as
/// bytes, keys as base64 strings of `flag || key`, and a threshold.
fn legacy_multisig<'a>(body: &'a [u8], bump: &'a Bump) -> Result<ParsedSignature<'a>, Error> {
    let bad = || malformed("invalid multisig");
    let mut r = Reader::new(body);
    let sigs_start = r.pos();
    let n = r
        .seq_len(CompressedSignature::MIN_WIRE_SIZE)
        .map_err(|_| bad())?;
    let mut sigs = containers::Vec::with_capacity_in(n, bump);
    for _ in 0..n {
        sigs.push(CompressedSignature::parse(&mut r).map_err(|_| bad())?);
    }
    let sigs_len = r.pos() - sigs_start;
    let roaring_bytes = r.byte_vec().map_err(|_| bad())?;
    let n = r.seq_len(1 + 1).map_err(|_| bad())?;
    let mut pk_map = containers::Vec::with_capacity_in(n, bump);
    let mut pk_map_len = uleb_len(n);
    for _ in 0..n {
        let encoded = r.str().map_err(|_| bad())?;
        let weight = r.u8().map_err(|_| bad())?;
        let key = decode_legacy_key(encoded, bump).ok_or_else(bad)?;
        // The reference re-encodes the key; decoding is strict (canonical
        // padding and trailing bits), so the encoding comes back the same.
        pk_map_len += uleb_len(encoded.len()) + encoded.len() + 1;
        pk_map.push((key, weight));
    }
    let threshold = r.u16().map_err(|_| bad())?;
    r.finish().map_err(|_| bad())?;

    // `deserialize_sui_bitmap`: bounded cardinality, then rebuilt from its
    // values, which is what the reference serializes again.
    let bitmap = roaring::RoaringBitmap::deserialize_from(roaring_bytes).map_err(|_| bad())?;
    if bitmap.len() > MAX_ROARING_CARDINALITY {
        return Err(bad());
    }
    let canonical: roaring::RoaringBitmap = bitmap.iter().collect();
    // `bitmap_to_u16`.
    let mut bits = 0u16;
    for i in &canonical {
        if i >= 10 {
            return Err(bad());
        }
        bits |= 1 << i;
    }

    let multisig = MultiSig {
        sigs: sigs.leak(),
        bitmap: bits,
        multisig_pk: MultiSigPublicKey {
            pk_map: pk_map.leak(),
            threshold,
        },
    };
    if multisig.sigs.len() > multisig.multisig_pk.pk_map.len()
        || multisig.sigs.is_empty()
        || !multisig_pk_valid(&multisig.multisig_pk)
    {
        return Err(bad());
    }
    let bitmap_len = canonical.serialized_size();
    let serialized_len =
        1 + sigs_len + uleb_len(bitmap_len) + bitmap_len + pk_map_len + size_of::<u16>();
    Ok(ParsedSignature::MultiSigLegacy {
        multisig,
        serialized_len,
    })
}

/// `PublicKey::decode_base64`: a flag, then a key fastcrypto accepts. zkLogin
/// keys have no base64 form.
fn decode_legacy_key<'a>(encoded: &str, bump: &'a Bump) -> Option<PublicKey<'a>> {
    use base64ct::Encoding as _;
    // The longest key and its flag. A longer string fails here where the
    // reference fails on the key's length: both reject.
    let mut buf = [0u8; 34];
    let bytes = base64ct::Base64::decode(encoded, &mut buf).ok()?;
    let (&flag, key) = bytes.split_first()?;
    let valid = match flag {
        0 => fastcrypto::ed25519::Ed25519PublicKey::from_bytes(key).is_ok(),
        1 => fastcrypto::secp256k1::Secp256k1PublicKey::from_bytes(key).is_ok(),
        2 | 6 => fastcrypto::secp256r1::Secp256r1PublicKey::from_bytes(key).is_ok(),
        _ => false,
    };
    if !valid {
        return None;
    }
    let copy = |key: &[u8]| -> &'a [u8] {
        let mut v = containers::Vec::with_capacity_in(key.len(), bump);
        v.extend_from_slice(key);
        v.leak()
    };
    Some(match flag {
        0 => PublicKey::Ed25519(copy(key).try_into().ok()?),
        1 => PublicKey::Secp256k1(copy(key).try_into().ok()?),
        2 => PublicKey::Secp256r1(copy(key).try_into().ok()?),
        _ => PublicKey::Passkey(copy(key).try_into().ok()?),
    })
}

/// The length of `n` as a uleb128 prefix.
pub(crate) fn uleb_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}
