// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signature verification: `verify_sender_signed_data_message_signatures`
//! and each scheme's `verify_authenticator`. Signatures are the ones
//! `sender_signed::validity_check` parsed.

use blake2::Blake2b;
use blake2::digest::consts::U32;
use containers::Bump;
use fastcrypto::ed25519::{Ed25519PublicKey, Ed25519Signature};
use fastcrypto::hash::{HashFunction, Sha256};
use fastcrypto::secp256k1::{Secp256k1PublicKey, Secp256k1Signature};
use fastcrypto::secp256r1::{Secp256r1PublicKey, Secp256r1Signature};
use fastcrypto::traits::{ToFromBytes, VerifyingKey};
use fastcrypto_zkp::bn254::zk_login::{JWK, JwkId, OIDCProvider};
use fastcrypto_zkp::bn254::zk_login_api::{ZkLoginCircuitMode, ZkLoginEnv};
use messages::base::SuiAddress;
use messages::signature::{CompressedSignature, MultiSig, PublicKey};
use messages::transaction::{SenderSignedData, TransactionKind};
use protocol_config::{Chain, ProtocolConfig};

use crate::signature::{ParsedSignature, Passkey, ZkLoginAuthenticator, zklogin};
use crate::{Error, ErrorKind};

/// What verification needs from the epoch, as the reference's
/// `SignatureVerifier` holds it: the active JWKs and the verification
/// settings of the protocol config.
// The flags are the reference's `VerifyParams`, one per protocol setting.
#[allow(clippy::struct_excessive_bools)]
pub struct Verifier {
    jwks: imbl::HashMap<JwkId, JWK>,
    supported_providers: Vec<OIDCProvider>,
    zk_login_env: ZkLoginEnv,
    circuit_mode: ZkLoginCircuitMode,
    verify_legacy_zklogin_address: bool,
    accept_zklogin_in_multisig: bool,
    accept_passkey_in_multisig: bool,
    zklogin_max_epoch_upper_bound_delta: Option<u64>,
    additional_multisig_checks: bool,
    validate_zklogin_public_identifier: bool,
}

impl Verifier {
    pub fn new(
        config: &ProtocolConfig,
        chain: Chain,
        jwks: impl IntoIterator<Item = (JwkId, JWK)>,
    ) -> Verifier {
        Verifier {
            jwks: jwks.into_iter().collect(),
            supported_providers: config
                .zklogin_supported_providers()
                .iter()
                .map(|s| {
                    s.parse()
                        .expect("the protocol config names known providers")
                })
                .collect(),
            // Testnet shares mainnet's proving key.
            zk_login_env: match chain {
                Chain::Mainnet | Chain::Testnet => ZkLoginEnv::Prod,
                Chain::Unknown => ZkLoginEnv::Test,
            },
            circuit_mode: match config.zklogin_circuit_mode() {
                0 => ZkLoginCircuitMode::V1Only,
                1 => ZkLoginCircuitMode::Both,
                2 => ZkLoginCircuitMode::V2Only,
                mode => panic!("unknown zkLogin circuit mode {mode}"),
            },
            verify_legacy_zklogin_address: config.verify_legacy_zklogin_address(),
            accept_zklogin_in_multisig: config.accept_zklogin_in_multisig(),
            accept_passkey_in_multisig: config.accept_passkey_in_multisig(),
            zklogin_max_epoch_upper_bound_delta: config.zklogin_max_epoch_upper_bound_delta(),
            additional_multisig_checks: config.additional_multisig_checks(),
            validate_zklogin_public_identifier: config.validate_zklogin_public_identifier(),
        }
    }
}

fn invalid(what: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidSignature, what)
}

fn blake2b(parts: &[&[u8]]) -> [u8; 32] {
    use blake2::Digest as _;
    let mut hasher = Blake2b::<U32>::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// One signature per required signer, each signer's present, all valid.
/// `aliases` are the addresses each signer may sign as instead, which the
/// caller reads from the store; empty means none.
pub fn verify_signatures(
    tx: &SenderSignedData<'_>,
    signatures: &[ParsedSignature<'_>],
    epoch: u64,
    verifier: &Verifier,
    aliases: &[(SuiAddress, &[SuiAddress])],
    bump: &Bump,
) -> Result<(), Error> {
    let data = &tx.data;
    let sponsor = (data.gas_data.owner != data.sender).then_some(*data.gas_data.owner);
    let required = [Some(*data.sender), sponsor];
    let required_count = required.iter().flatten().count();
    if signatures.len() != required_count {
        return Err(Error::new(
            ErrorKind::SignerSignatureNumberMismatch,
            format!(
                "{} signatures for {required_count} signers",
                signatures.len()
            ),
        ));
    }
    // User transactions were checked not to be system transactions.
    if !matches!(data.kind, TransactionKind::ProgrammableTransaction(_)) {
        return Ok(());
    }

    // Signer by address, later signatures replacing earlier ones for the
    // same address, then walked in address order, as the reference's
    // `BTreeMap`.
    let mut by_signer = containers::Vec::with_capacity_in(2 * signatures.len(), bump);
    let mut insert =
        |address: SuiAddress, sig: usize| match by_signer.iter_mut().find(|(a, _)| *a == address) {
            Some(entry) => *entry = (address, sig),
            None => by_signer.push((address, sig)),
        };
    for (i, sig) in signatures.iter().enumerate() {
        if verifier.verify_legacy_zklogin_address
            && let ParsedSignature::ZkLogin(bytes) = sig
        {
            let zk = zklogin(&bytes[1..]).expect("parsed before");
            insert(zklogin_padded_address(&zk.inputs), i);
        }
        insert(signer_address(sig)?, i);
    }
    by_signer.sort_unstable_by_key(|(address, _)| address.0);

    for signer in required.iter().flatten() {
        let accepted = aliases
            .iter()
            .find(|(a, _)| a == signer)
            .map_or(std::slice::from_ref(signer), |(_, alias)| alias);
        if !accepted
            .iter()
            .any(|a| by_signer.iter().any(|(s, _)| s == a))
        {
            return Err(Error::new(
                ErrorKind::SignerSignatureAbsent,
                "no signature from a required signer",
            ));
        }
    }

    // The intent message is the intent's three bytes, then the data.
    let digest = blake2b(&[&[0, 0, 0], data.bytes]);
    for (address, i) in &by_signer {
        verify_authenticator(&signatures[*i], address, epoch, &digest, verifier)?;
    }
    Ok(())
}

/// The address a signature signs for.
fn signer_address(sig: &ParsedSignature<'_>) -> Result<SuiAddress, Error> {
    Ok(match sig {
        ParsedSignature::Simple(bytes) => {
            let (pk, len) = canonical_key(bytes[0], &bytes[65..])
                .ok_or_else(|| invalid("cannot parse public key"))?;
            SuiAddress(blake2b(&[&[bytes[0]], &pk[..len]]))
        }
        ParsedSignature::MultiSig(m) | ParsedSignature::MultiSigLegacy { multisig: m, .. } => {
            multisig_address(m)
        }
        ParsedSignature::ZkLogin(bytes) => {
            let zk = zklogin(&bytes[1..]).expect("parsed before");
            zklogin_address(&zk.inputs)
        }
        ParsedSignature::Passkey(p) => passkey_address(p),
    })
}

/// A key as fastcrypto re-encodes it once parsed, which is what addresses
/// hash: a Secp256r1 key given in SEC1's compact form hashes compressed.
/// The key is the first `len` bytes.
fn canonical_key(flag: u8, key: &[u8]) -> Option<([u8; 33], usize)> {
    fn reencode<K: ToFromBytes>(key: &[u8]) -> Option<([u8; 33], usize)> {
        let key = K::from_bytes(key).ok()?;
        let bytes = key.as_bytes();
        let mut out = [0u8; 33];
        out.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some((out, bytes.len()))
    }
    match flag {
        0 => reencode::<Ed25519PublicKey>(key),
        1 => reencode::<Secp256k1PublicKey>(key),
        2 => reencode::<Secp256r1PublicKey>(key),
        _ => None,
    }
}

fn key_flag_and_bytes<'k>(pk: &PublicKey<'k>) -> (u8, &'k [u8]) {
    match *pk {
        PublicKey::Ed25519(k) => (0, &k[..]),
        PublicKey::Secp256k1(k) => (1, &k[..]),
        PublicKey::Secp256r1(k) => (2, &k[..]),
        PublicKey::ZkLogin(k) => (5, k),
        PublicKey::Passkey(k) => (6, &k[..]),
    }
}

/// `flag || threshold || (flag || key || weight)*`, keys as given.
fn multisig_address(m: &MultiSig<'_>) -> SuiAddress {
    use blake2::Digest as _;
    let mut hasher = Blake2b::<U32>::new();
    hasher.update([3]);
    hasher.update(m.multisig_pk.threshold.to_le_bytes());
    for (pk, weight) in m.multisig_pk.pk_map {
        let (flag, bytes) = key_flag_and_bytes(pk);
        hasher.update([flag]);
        hasher.update(bytes);
        hasher.update([*weight]);
    }
    SuiAddress(hasher.finalize().into())
}

/// `flag || iss_len || iss || address_seed`, the seed without leading
/// zeros. The length is a `u8`, truncated as the reference truncates it.
fn zklogin_address(inputs: &fastcrypto_zkp::bn254::zk_login::ZkLoginInputs) -> SuiAddress {
    let iss = inputs.get_iss().as_bytes();
    SuiAddress(blake2b(&[
        &[5, iss.len() as u8],
        iss,
        inputs.get_address_seed().unpadded(),
    ]))
}

/// The legacy derivation: the public identifier, seed padded to 32 bytes.
fn zklogin_padded_address(inputs: &fastcrypto_zkp::bn254::zk_login::ZkLoginInputs) -> SuiAddress {
    let iss = inputs.get_iss().as_bytes();
    SuiAddress(blake2b(&[
        &[5, iss.len() as u8],
        iss,
        inputs.get_address_seed().padded(),
    ]))
}

fn passkey_address(p: &Passkey<'_>) -> SuiAddress {
    let (pk, len) = canonical_key(2, p.public_key).expect("checked when parsed");
    SuiAddress(blake2b(&[&[6], &pk[..len]]))
}

/// `GenericSignature::verify_authenticator`: the epoch, then the claims.
fn verify_authenticator(
    sig: &ParsedSignature<'_>,
    author: &SuiAddress,
    epoch: u64,
    digest: &[u8; 32],
    verifier: &Verifier,
) -> Result<(), Error> {
    match sig {
        ParsedSignature::Simple(bytes) => verify_simple(bytes, Some(author), digest),
        ParsedSignature::MultiSig(m) | ParsedSignature::MultiSigLegacy { multisig: m, .. } => {
            for s in m.sigs {
                if let CompressedSignature::ZkLogin(z) = s {
                    let zk = parse_nested_zklogin(z)?;
                    zklogin_epoch(&zk, epoch, verifier)?;
                }
            }
            verify_multisig(m, author, digest, verifier)
        }
        ParsedSignature::ZkLogin(bytes) => {
            let zk = zklogin(&bytes[1..]).expect("parsed before");
            zklogin_epoch(&zk, epoch, verifier)?;
            verify_zklogin(&zk, author, digest, verifier)
        }
        ParsedSignature::Passkey(p) => verify_passkey(p, author, digest),
    }
}

/// `Signature::verify_secure`. `author` is `None` for a zkLogin's ephemeral
/// signature, which does not sign for its own key's address.
fn verify_simple(
    bytes: &[u8],
    author: Option<&SuiAddress>,
    digest: &[u8; 32],
) -> Result<(), Error> {
    let (flag, sig, pk) = (bytes[0], &bytes[1..65], &bytes[65..]);
    let key_error = || Error::new(ErrorKind::KeyConversionError, "invalid public key");
    let sig_error = || invalid("cannot parse signature");
    macro_rules! check {
        ($pk:ty, $sig:ty) => {{
            let pk = <$pk>::from_bytes(pk).map_err(|_| key_error())?;
            let sig = <$sig>::from_bytes(sig).map_err(|_| sig_error())?;
            if let Some(author) = author
                && SuiAddress(blake2b(&[&[flag], pk.as_bytes()])) != *author
            {
                return Err(Error::new(ErrorKind::IncorrectSigner, "incorrect signer"));
            }
            pk.verify(digest, &sig)
                .map_err(|e| invalid(format!("signature does not verify: {e}")))
        }};
    }
    match flag {
        0 => check!(Ed25519PublicKey, Ed25519Signature),
        1 => check!(Secp256k1PublicKey, Secp256k1Signature),
        _ => check!(Secp256r1PublicKey, Secp256r1Signature),
    }
}

/// `MultiSig::verify_claims`.
fn verify_multisig(
    m: &MultiSig<'_>,
    author: &SuiAddress,
    digest: &[u8; 32],
    verifier: &Verifier,
) -> Result<(), Error> {
    // Parsing validated the key set.
    if verifier.validate_zklogin_public_identifier {
        for (pk, _) in m.multisig_pk.pk_map {
            if let PublicKey::ZkLogin(id) = pk {
                validate_zklogin_identifier(id)?;
            }
        }
    }
    if multisig_address(m) != *author {
        return Err(invalid("address does not match the keys"));
    }
    if !verifier.accept_zklogin_in_multisig
        && m.sigs
            .iter()
            .any(|s| matches!(s, CompressedSignature::ZkLogin(_)))
    {
        return Err(invalid("zkLogin not accepted in multisig"));
    }
    if !verifier.accept_passkey_in_multisig
        && m.sigs
            .iter()
            .any(|s| matches!(s, CompressedSignature::Passkey(_)))
    {
        return Err(invalid("passkey not accepted in multisig"));
    }

    // Signatures pair with the bitmap's set bits in order. The reference
    // zips the two with `zip_debug_eq`, which in release stops at the
    // shorter: signatures past the set bits are ignored.
    let indices = (0..10u16).filter(|i| m.bitmap & (1 << i) != 0);
    let mut weight: u16 = 0;
    #[allow(clippy::disallowed_methods)] // Truncating is the reference's behavior.
    for (sig, i) in m.sigs.iter().zip(indices) {
        let Some((pk, w)) = m.multisig_pk.pk_map.get(usize::from(i)) else {
            return Err(invalid("bitmap names a missing key"));
        };
        let (pk_flag, pk_bytes) = key_flag_and_bytes(pk);
        let scheme_matches = |flag| !verifier.additional_multisig_checks || pk_flag == flag;
        let sub_author = SuiAddress(blake2b(&[&[pk_flag], pk_bytes]));
        let ok = match sig {
            CompressedSignature::Ed25519(s) => {
                if !scheme_matches(0) {
                    return Err(invalid("signature and key schemes differ"));
                }
                verify_raw::<Ed25519PublicKey>(pk_bytes, &s[..], digest)?
            }
            CompressedSignature::Secp256k1(s) => {
                if !scheme_matches(1) {
                    return Err(invalid("signature and key schemes differ"));
                }
                verify_raw::<Secp256k1PublicKey>(pk_bytes, &s[..], digest)?
            }
            CompressedSignature::Secp256r1(s) => {
                if !scheme_matches(2) {
                    return Err(invalid("signature and key schemes differ"));
                }
                verify_raw::<Secp256r1PublicKey>(pk_bytes, &s[..], digest)?
            }
            CompressedSignature::ZkLogin(z) => {
                if !scheme_matches(5) {
                    return Err(invalid("signature and key schemes differ"));
                }
                let zk = parse_nested_zklogin(z)?;
                verify_zklogin(&zk, &sub_author, digest, verifier).is_ok()
            }
            // The reference checks no scheme here.
            CompressedSignature::Passkey(p) => {
                let p = parse_nested_passkey(p)?;
                verify_passkey(&p, &sub_author, digest).is_ok()
            }
        };
        if !ok {
            return Err(invalid("a multisig member's signature does not verify"));
        }
        weight += u16::from(*w);
    }
    if weight < m.multisig_pk.threshold {
        return Err(invalid("insufficient weight"));
    }
    Ok(())
}

/// A member signature: the key and signature must parse (else an error)
/// and then verify (else `false`).
fn verify_raw<P>(pk: &[u8], sig: &[u8], digest: &[u8; 32]) -> Result<bool, Error>
where
    P: VerifyingKey + ToFromBytes,
    P::Sig: ToFromBytes,
{
    let pk = P::from_bytes(pk).map_err(|_| invalid("invalid member public key"))?;
    let sig = P::Sig::from_bytes(sig).map_err(|_| invalid("invalid member signature"))?;
    Ok(pk.verify(digest, &sig).is_ok())
}

fn parse_nested_zklogin(bytes: &[u8]) -> Result<ZkLoginAuthenticator, Error> {
    match bytes.split_first() {
        Some((5, body)) => zklogin(body),
        _ => None,
    }
    .ok_or_else(|| invalid("invalid zkLogin authenticator bytes"))
}

fn parse_nested_passkey(bytes: &[u8]) -> Result<Passkey<'_>, Error> {
    match bytes.split_first() {
        Some((6, body)) => crate::signature::passkey(body),
        _ => None,
    }
    .ok_or_else(|| invalid("invalid passkey authenticator bytes"))
}

/// `ZkLoginPublicIdentifier::validate`: `iss_len || iss || seed`, the issuer
/// UTF-8 and the seed at most 32 bytes.
fn validate_zklogin_identifier(id: &[u8]) -> Result<(), Error> {
    let bad = || invalid("invalid zkLogin public identifier");
    let (&iss_len, rest) = id.split_first().ok_or_else(bad)?;
    let iss = rest.get(..usize::from(iss_len)).ok_or_else(bad)?;
    std::str::from_utf8(iss).map_err(|_| bad())?;
    if rest.len() - usize::from(iss_len) > 32 {
        return Err(bad());
    }
    Ok(())
}

/// `current + delta >= max_epoch >= current`.
fn zklogin_epoch(zk: &ZkLoginAuthenticator, epoch: u64, verifier: &Verifier) -> Result<(), Error> {
    if let Some(delta) = verifier.zklogin_max_epoch_upper_bound_delta
        && zk.max_epoch > epoch + delta
    {
        return Err(invalid("zkLogin max epoch too far ahead"));
    }
    if epoch > zk.max_epoch {
        return Err(invalid("zkLogin signature expired"));
    }
    Ok(())
}

/// `ZkLoginAuthenticator::verify_claims`, without the reference's cache of
/// verified proofs.
fn verify_zklogin(
    zk: &ZkLoginAuthenticator,
    author: &SuiAddress,
    digest: &[u8; 32],
    verifier: &Verifier,
) -> Result<(), Error> {
    if zklogin_address(&zk.inputs) != *author
        && (!verifier.verify_legacy_zklogin_address
            || zklogin_padded_address(&zk.inputs) != *author)
    {
        return Err(Error::new(
            ErrorKind::InvalidAddress,
            "zkLogin address mismatch",
        ));
    }
    if !verifier.supported_providers.is_empty() {
        let provider =
            OIDCProvider::from_iss(zk.inputs.get_iss()).map_err(|_| invalid("unknown provider"))?;
        if !verifier.supported_providers.contains(&provider) {
            return Err(invalid("provider not supported"));
        }
    }
    verify_simple(&zk.user_signature, None, digest)?;

    let mut extended_pk = Vec::with_capacity(34);
    extended_pk.push(zk.user_signature[0]);
    extended_pk.extend_from_slice(&zk.user_signature[65..]);
    fastcrypto_zkp::bn254::zk_login_api::verify_zk_login(
        &zk.inputs,
        zk.max_epoch,
        &extended_pk,
        &verifier.jwks,
        &verifier.zk_login_env,
        verifier.circuit_mode,
    )
    .map_err(|e| invalid(format!("zkLogin proof: {e}")))
}

/// `PasskeyAuthenticator::verify_claims`.
fn verify_passkey(p: &Passkey<'_>, author: &SuiAddress, digest: &[u8; 32]) -> Result<(), Error> {
    if passkey_address(p) != *author {
        return Err(invalid("invalid author"));
    }
    if p.challenge != *digest {
        return Err(invalid("invalid challenge"));
    }
    let client_data_hash = Sha256::digest(p.client_data_json.as_bytes()).digest;
    let mut message = Vec::with_capacity(p.authenticator_data.len() + 32);
    message.extend_from_slice(p.authenticator_data);
    message.extend_from_slice(&client_data_hash);
    let pk = Secp256r1PublicKey::from_bytes(p.public_key).expect("checked when parsed");
    let sig = Secp256r1Signature::from_bytes(p.signature).expect("checked when parsed");
    pk.verify(&message, &sig)
        .map_err(|_| invalid("fails to verify"))
}
