// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signature verification vectors: transactions signed with every scheme,
//! then broken, verified by the reference's
//! `verify_sender_signed_data_message_signatures`. zkLogin uses sui's test
//! proofs and JWKs, which verify only in the test environment (the Unknown
//! chain); `jwks()` is written out for the runner.

use std::sync::Arc;

use fastcrypto::hash::{HashFunction, Sha256};
use fastcrypto::traits::{KeyPair as _, Signer, ToFromBytes};
use fastcrypto_zkp::bn254::zk_login::{JWK, JwkId, OIDCProvider, ZkLoginInputs, parse_jwks};
use fastcrypto_zkp::bn254::zk_login_api::ZkLoginEnv;
use rand::SeedableRng;
use rand::rngs::StdRng;
use shared_crypto::intent::{Intent, IntentMessage};
use sui_protocol_config::{Chain, ProtocolConfig};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::{DefaultHash, PublicKey, Signature, SuiKeyPair, get_key_pair_from_rng};
use sui_types::multisig::{MultiSig, MultiSigPublicKey};
use sui_types::multisig_legacy::{MultiSigLegacy, MultiSigPublicKeyLegacy};
use sui_types::signature::{GenericSignature, VerifyParams};
use sui_types::signature_verification::{
    VerifiedDigestCache, verify_sender_signed_data_message_signatures,
};
use sui_types::transaction::{SenderSignedData, TransactionData};
use sui_types::zk_login_authenticator::ZkLoginAuthenticator;

use crate::validity::{Spec, verdict_of};
use crate::validity_signed::signed;

const DEFAULT_JWK: &str = r#"{"keys":[{"alg":"RS256","e":"AQAB","kid":"1","kty":"RSA","n":"6lq9MQ-q6hcxr7kOUp-tHlHtdcDsVLwVIw13iXUCvuDOeCi0VSuxCCUY6UmMjy53dX00ih2E4Y4UvlrmmurK0eG26b-HMNNAvCGsVXHU3RcRhVoHDaOwHwU72j7bpHn9XbP3Q3jebX6KIfNbei2MiR0Wyb8RZHE-aZhRYO8_-k9G2GycTpvc-2GBsP8VHLUKKfAs2B6sW3q3ymU6M0L-cFXkZ9fHkn9ejs-sqZPhMJxtBPBxoUIUQFTgv4VXTSv914f_YkNw-EjuwbgwXMvpyr06EyfImxHoxsZkFYB-qBYHtaMxTnFsZBr6fn8Ha2JqT1hoP7Z5r5wxDu3GQhKkHw","use":"sig"}]}"#;
const DEFAULT_PROOF: &str = r#"{"proofPoints":{"a":["17318089125952421736342263717932719437717844282410187957984751939942898251250","11373966645469122582074082295985388258840681618268593976697325892280915681207","1"],"b":[["5939871147348834997361720122238980177152303274311047249905942384915768690895","4533568271134785278731234570361482651996740791888285864966884032717049811708"],["10564387285071555469753990661410840118635925466597037018058770041347518461368","12597323547277579144698496372242615368085801313343155735511330003884767957854"],["1","0"]],"c":["15791589472556826263231644728873337629015269984699404073623603352537678813171","4547866499248881449676161158024748060485373250029423904113017422539037162527","1"]},"issBase64Details":{"value":"wiaXNzIjoiaHR0cHM6Ly9pZC50d2l0Y2gudHYvb2F1dGgyIiw","indexMod4":2},"headerBase64":"eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6IjEifQ"}"#;
const DEFAULT_SEED: &str =
    "20794788559620669596206457022966176986688727876128223628113916380927502737911";

/// The JWKs every verification vector runs with.
pub(crate) fn jwks() -> Vec<(JwkId, JWK)> {
    parse_jwks(DEFAULT_JWK.as_bytes(), &OIDCProvider::Twitch, false).unwrap()
}

pub(crate) fn jwks_json() -> String {
    serde_json::to_string(&jwks()).unwrap()
}

fn params(config: &ProtocolConfig, chain: Chain) -> VerifyParams {
    VerifyParams::new(
        jwks().into_iter().collect(),
        config
            .zklogin_supported_providers()
            .iter()
            .map(|s| s.parse().unwrap())
            .collect(),
        match chain {
            Chain::Mainnet | Chain::Testnet => ZkLoginEnv::Prod,
            Chain::Unknown => ZkLoginEnv::Test,
        },
        config.zklogin_circuit_mode(),
        config.verify_legacy_zklogin_address(),
        config.accept_zklogin_in_multisig(),
        config.accept_passkey_in_multisig(),
        config.zklogin_max_epoch_upper_bound_delta(),
        config.additional_multisig_checks(),
        config.validate_zklogin_public_identifier(),
    )
}

pub(crate) fn verdict(bytes: &[u8], config: &ProtocolConfig, chain: Chain, epoch: u64) -> String {
    let Ok(tx) = bcs::from_bytes::<SenderSignedData>(bytes) else {
        return "TransactionDeserializationError".to_owned();
    };
    match verify_sender_signed_data_message_signatures(
        &tx,
        epoch,
        &params(config, chain),
        Arc::new(VerifiedDigestCache::new_empty()),
        vec![],
    ) {
        Ok(_) => "ok".to_owned(),
        Err(e) => verdict_of(e),
    }
}

fn digest(data: &TransactionData) -> [u8; 32] {
    let msg = IntentMessage::new(Intent::sui_transaction(), data);
    let mut hasher = DefaultHash::default();
    bcs::serialize_into(&mut hasher, &msg).unwrap();
    hasher.finalize().digest
}

fn sign(data: &TransactionData, key: &SuiKeyPair) -> Vec<u8> {
    let msg = IntentMessage::new(Intent::sui_transaction(), data);
    Signature::new_secure(&msg, key).as_ref().to_vec()
}

fn address(key: &SuiKeyPair) -> SuiAddress {
    SuiAddress::from(&key.public())
}

fn from(sender: SuiAddress) -> TransactionData {
    Spec {
        sender,
        owner: sender,
        ..Spec::new()
    }
    .build()
}

fn generic(sig: &GenericSignature) -> Vec<u8> {
    sig.as_ref().to_vec()
}

pub(crate) fn cases() -> Vec<(String, Vec<u8>)> {
    let mut rng = StdRng::from_seed([11; 32]);
    let ed = SuiKeyPair::Ed25519(get_key_pair_from_rng(&mut rng).1);
    let k1 = SuiKeyPair::Secp256k1(get_key_pair_from_rng(&mut rng).1);
    let r1 = SuiKeyPair::Secp256r1(get_key_pair_from_rng(&mut rng).1);
    let other = SuiKeyPair::Ed25519(get_key_pair_from_rng(&mut rng).1);

    let mut cases: Vec<(String, Vec<u8>)> = vec![];
    let mut add = |label: &str, data: &TransactionData, sigs: &[&[u8]]| {
        cases.push((format!("verify_{label}"), signed([0, 0, 0], data, sigs)));
    };

    // Single keys.
    for (label, key) in [("ed25519", &ed), ("k1", &k1), ("r1", &r1)] {
        let data = from(address(key));
        let sig = sign(&data, key);
        add(label, &data, &[&sig]);
        let mut flipped = sig.clone();
        flipped[10] ^= 1;
        add(&format!("{label}_flipped"), &data, &[&flipped]);
        add(
            &format!("{label}_other_signer"),
            &data,
            &[&sign(&data, &other)],
        );
        add(&format!("{label}_twice"), &data, &[&sig, &sig]);
    }
    let data = from(address(&ed));
    add("no_signatures", &data, &[]);
    let mut bad_key = sign(&data, &ed);
    bad_key[65..].fill(0xff);
    add("ed25519_bad_key", &data, &[&bad_key]);

    // Sponsored: sender and gas owner both sign.
    let sponsored = Spec {
        sender: address(&ed),
        owner: address(&k1),
        ..Spec::new()
    }
    .build();
    let (s_ed, s_k1) = (sign(&sponsored, &ed), sign(&sponsored, &k1));
    add("sponsored", &sponsored, &[&s_ed, &s_k1]);
    add("sponsored_reversed", &sponsored, &[&s_k1, &s_ed]);
    add("sponsored_sender_only", &sponsored, &[&s_ed]);
    add("sponsored_sender_twice", &sponsored, &[&s_ed, &s_ed]);

    // Multisig: 2-of-3 over the three schemes.
    let pks = vec![ed.public(), k1.public(), r1.public()];
    let msig_pk = MultiSigPublicKey::new(pks.clone(), vec![1, 1, 1], 2).unwrap();
    let msig_addr = SuiAddress::from(&msig_pk);
    let data = from(msig_addr);
    let member = |key: &SuiKeyPair| {
        GenericSignature::Signature(Signature::new_secure(
            &IntentMessage::new(Intent::sui_transaction(), &data),
            key,
        ))
    };
    let combine = |keys: &[&SuiKeyPair]| {
        let sigs = keys.iter().map(|k| member(k)).collect();
        generic(&GenericSignature::MultiSig(
            MultiSig::combine(sigs, msig_pk.clone()).unwrap(),
        ))
    };
    add("multisig_2_of_3", &data, &[&combine(&[&ed, &k1])]);
    add("multisig_3_of_3", &data, &[&combine(&[&ed, &k1, &r1])]);
    add("multisig_1_of_3", &data, &[&combine(&[&r1])]);
    // Bitmap and signatures out of step: the reference pairs them up to the
    // shorter and ignores the rest.
    // A multisig built by hand: each signature's flag doubles as its
    // `CompressedSignature` variant, then the bitmap and the key set.
    let raw = |sigs: Vec<Vec<u8>>, bitmap: u16| {
        let mut b = vec![3, sigs.len() as u8];
        for s in &sigs {
            b.push(s[0]);
            b.extend_from_slice(&s[1..65]);
        }
        b.extend(bitmap.to_le_bytes());
        b.extend(bcs::to_bytes(&msig_pk).unwrap());
        b
    };
    let s_ed = sign(&data, &ed);
    let s_k1 = sign(&data, &k1);
    let s_r1 = sign(&data, &r1);
    add(
        "multisig_raw_ok",
        &data,
        &[&raw(vec![s_ed.clone(), s_k1.clone()], 0b011)],
    );
    add(
        "multisig_extra_sig_ignored",
        &data,
        &[&raw(vec![s_ed.clone(), s_k1.clone(), s_ed.clone()], 0b011)],
    );
    add(
        "multisig_extra_bit",
        &data,
        &[&raw(vec![s_ed.clone(), s_k1.clone()], 0b111)],
    );
    add(
        "multisig_garbage_past_bits",
        &data,
        &[&raw(vec![s_ed.clone(), s_k1.clone(), vec![0; 65]], 0b011)],
    );
    add(
        "multisig_wrong_order",
        &data,
        &[&raw(vec![s_k1.clone(), s_ed.clone()], 0b011)],
    );
    add(
        "multisig_scheme_mismatch",
        &data,
        &[&raw(vec![s_ed.clone(), s_ed.clone()], 0b011)],
    );
    add(
        "multisig_bit_past_keys",
        &data,
        &[&raw(vec![s_ed.clone(), s_r1.clone()], 0b1001)],
    );
    // The legacy format, converted by the reference before verifying.
    let legacy_pk = MultiSigPublicKeyLegacy::new(pks.clone(), vec![1, 1, 1], 2).unwrap();
    let legacy = MultiSigLegacy::combine(vec![member(&ed), member(&r1)], legacy_pk).unwrap();
    add(
        "multisig_legacy",
        &data,
        &[&generic(&GenericSignature::MultiSigLegacy(legacy))],
    );
    let other_data = from(address(&ed));
    add(
        "multisig_wrong_sender",
        &other_data,
        &[&combine(&[&ed, &k1])],
    );

    // Passkey: the challenge is the transaction's digest.
    let r1_fc = match &r1 {
        SuiKeyPair::Secp256r1(k) => k.copy(),
        _ => unreachable!(),
    };
    let passkey_pk = PublicKey::Passkey((&r1_fc.public().clone()).into());
    let passkey_addr = SuiAddress::from(&passkey_pk);
    let passkey = |challenge: [u8; 32], auth: &[u8], tamper: bool| {
        let json = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://example.com"}}"#,
            <base64ct::Base64UrlUnpadded as base64ct::Encoding>::encode_string(&challenge)
        );
        let mut msg = auth.to_vec();
        msg.extend_from_slice(&Sha256::digest(json.as_bytes()).digest);
        let sig: fastcrypto::secp256r1::Secp256r1Signature = r1_fc.sign(&msg);
        let mut user_sig = vec![2];
        user_sig.extend_from_slice(sig.as_bytes());
        user_sig.extend_from_slice(r1_fc.public().as_bytes());
        if tamper {
            user_sig[5] ^= 1;
        }
        let mut b = vec![6];
        b.extend(bcs::to_bytes(&(auth.to_vec(), json, user_sig)).unwrap());
        b
    };
    let data = from(passkey_addr);
    let ok = passkey(digest(&data), &[1; 37], false);
    add("passkey", &data, &[&ok]);
    add(
        "passkey_wrong_challenge",
        &data,
        &[&passkey([9; 32], &[1; 37], false)],
    );
    add(
        "passkey_bad_signature",
        &data,
        &[&passkey(digest(&data), &[1; 37], true)],
    );
    add("passkey_wrong_sender", &from(address(&ed)), &[&ok]);

    // zkLogin: sui's default test proof, ephemeral key from seed [0; 32],
    // max epoch 10.
    let inputs = ZkLoginInputs::from_json(DEFAULT_PROOF, DEFAULT_SEED).unwrap();
    let eph = SuiKeyPair::Ed25519(fastcrypto::ed25519::Ed25519KeyPair::generate(
        &mut StdRng::from_seed([0; 32]),
    ));
    let zk_addr = SuiAddress::try_from_unpadded(&inputs).unwrap();
    let zk_sig = |data: &TransactionData, key: &SuiKeyPair, max_epoch: u64| {
        let s = Signature::new_secure(&IntentMessage::new(Intent::sui_transaction(), data), key);
        generic(&GenericSignature::ZkLoginAuthenticator(
            ZkLoginAuthenticator::new(inputs.clone(), max_epoch, s),
        ))
    };
    let data = from(zk_addr);
    add("zklogin", &data, &[&zk_sig(&data, &eph, 10)]);
    add(
        "zklogin_wrong_ephemeral_key",
        &data,
        &[&zk_sig(&data, &other, 10)],
    );
    add(
        "zklogin_max_epoch_changed",
        &data,
        &[&zk_sig(&data, &eph, 11)],
    );
    let padded = from(SuiAddress::try_from_padded(&inputs).unwrap());
    add(
        "zklogin_padded_address",
        &padded,
        &[&zk_sig(&padded, &eph, 10)],
    );
    add(
        "zklogin_wrong_sender",
        &from(address(&ed)),
        &[&zk_sig(&data, &eph, 10)],
    );
    cases
}
