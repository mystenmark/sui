// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Signature parsing vectors: `GenericSignature` bytes, valid and broken in
//! each way the reference's `from_bytes` checks, with its verdict. One per
//! line:
//!
//! ```text
//! <label> <hex> <verdict>
//! ```
//!
//! The verdict is `error`, or the `GenericSignature` variant and the length
//! of its re-serialization (`as_ref()`), which is what the transaction size
//! limit counts: `MultiSigLegacy:231`.

use std::fmt::Write as _;

use fastcrypto::secp256k1::Secp256k1KeyPair;
use fastcrypto::secp256r1::Secp256r1KeyPair;
use fastcrypto::traits::{KeyPair as _, ToFromBytes};
use rand::SeedableRng;
use rand::rngs::StdRng;
use sui_types::crypto::{SuiKeyPair, get_key_pair_from_rng};
use sui_types::signature::GenericSignature;

use crate::hex;

/// A BCS writer, for encodings serde cannot produce (arrays over 32).
#[derive(Default, Clone)]
struct W(Vec<u8>);

impl W {
    fn uleb(mut self, mut v: usize) -> W {
        while v >= 0x80 {
            self.0.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
        self
    }
    fn raw(mut self, b: &[u8]) -> W {
        self.0.extend_from_slice(b);
        self
    }
    fn bytes(self, b: &[u8]) -> W {
        self.uleb(b.len()).raw(b)
    }
    fn u16(self, v: u16) -> W {
        self.raw(&v.to_le_bytes())
    }
}

/// A compressed signature: Ed25519, Secp256k1 or Secp256r1 by variant.
fn compressed(variant: usize) -> W {
    W::default().uleb(variant).raw(&[variant as u8 + 0x40; 64])
}

/// Raw key bytes by scheme flag, BCS-encoded as `crypto::PublicKey`.
fn public_key(flag: u8, key: &[u8]) -> W {
    let variant = match flag {
        0 => 0,
        1 => 1,
        2 => 2,
        6 => 4,
        _ => unreachable!(),
    };
    W::default().uleb(variant).raw(key)
}

fn multisig(sigs: &[W], bitmap: u16, pks: &[(W, u8)], threshold: u16) -> Vec<u8> {
    let mut w = W::default().raw(&[3]).uleb(sigs.len());
    for s in sigs {
        w = w.raw(&s.0);
    }
    w = w.u16(bitmap).uleb(pks.len());
    for (pk, weight) in pks {
        w = w.raw(&pk.0).raw(&[*weight]);
    }
    w.u16(threshold).0
}

fn legacy(sigs: &[W], bitmap: &[u8], pks: &[(String, u8)], threshold: u16) -> Vec<u8> {
    let mut w = W::default().raw(&[3]).uleb(sigs.len());
    for s in sigs {
        w = w.raw(&s.0);
    }
    w = w.bytes(bitmap).uleb(pks.len());
    for (pk, weight) in pks {
        w = w.bytes(pk.as_bytes()).raw(&[*weight]);
    }
    w.u16(threshold).0
}

fn roaring(values: &[u32]) -> Vec<u8> {
    let mut bitmap = roaring::RoaringBitmap::new();
    for v in values {
        bitmap.insert(*v);
    }
    let mut out = vec![];
    bitmap.serialize_into(&mut out).unwrap();
    out
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn verdict(bytes: &[u8]) -> String {
    match GenericSignature::from_bytes(bytes) {
        Err(_) => "error".to_owned(),
        Ok(sig) => {
            let variant = match &sig {
                GenericSignature::Signature(_) => "Signature",
                GenericSignature::MultiSig(_) => "MultiSig",
                GenericSignature::MultiSigLegacy(_) => "MultiSigLegacy",
                GenericSignature::ZkLoginAuthenticator(_) => "ZkLoginAuthenticator",
                GenericSignature::PasskeyAuthenticator(_) => "PasskeyAuthenticator",
            };
            format!("{variant}:{}", sig.as_ref().len())
        }
    }
}

pub fn vectors() -> String {
    let mut cases: Vec<(String, Vec<u8>)> = vec![];
    let mut add = |label: &str, bytes: Vec<u8>| cases.push((label.to_owned(), bytes));

    add("empty", vec![]);
    for flag in [0u8, 1, 2, 4, 7, 0xff] {
        for len in [96, 97, 98, 99] {
            let mut b = vec![flag];
            b.resize(len, 0x33);
            add(&format!("simple_{flag}_{len}"), b);
        }
    }

    // Keys: points are not checked in the new format, but are in the legacy one.
    let mut rng = StdRng::from_seed([3; 32]);
    let ed = || {
        let (_, kp): (_, fastcrypto::ed25519::Ed25519KeyPair) =
            get_key_pair_from_rng(&mut StdRng::from_seed([4; 32]));
        kp.public().as_bytes().to_vec()
    };
    let (_, k1): (_, Secp256k1KeyPair) = get_key_pair_from_rng(&mut rng);
    let (_, r1): (_, Secp256r1KeyPair) = get_key_pair_from_rng(&mut rng);
    let k1 = k1.public().as_bytes().to_vec();
    let r1 = r1.public().as_bytes().to_vec();
    let ed = ed();

    let pk = |i: u8| public_key(0, &[i; 32]);
    let two = [(pk(1), 1), (pk(2), 1)];
    add("multisig_ok", multisig(&[compressed(0)], 0b1, &two, 1));
    add(
        "multisig_two_sigs",
        multisig(&[compressed(0), compressed(1)], 0b11, &two, 2),
    );
    add("multisig_no_sigs", multisig(&[], 0b1, &two, 1));
    add(
        "multisig_more_sigs_than_keys",
        multisig(
            &[compressed(0), compressed(0), compressed(0)],
            0b111,
            &two,
            1,
        ),
    );
    add(
        "multisig_bitmap_3ff",
        multisig(&[compressed(0)], 0x3ff, &two, 1),
    );
    add(
        "multisig_bitmap_400",
        multisig(&[compressed(0)], 0x400, &two, 1),
    );
    add(
        "multisig_threshold_0",
        multisig(&[compressed(0)], 0b1, &two, 0),
    );
    add(
        "multisig_threshold_over",
        multisig(&[compressed(0)], 0b1, &two, 3),
    );
    add("multisig_no_keys", multisig(&[compressed(0)], 0b1, &[], 1));
    add(
        "multisig_zero_weight",
        multisig(&[compressed(0)], 0b1, &[(pk(1), 0), (pk(2), 1)], 1),
    );
    add(
        "multisig_duplicate_key",
        multisig(&[compressed(0)], 0b1, &[(pk(1), 1), (pk(1), 1)], 1),
    );
    for n in [10u8, 11] {
        let keys: Vec<_> = (0..n).map(|i| (pk(i + 1), 1)).collect();
        add(
            &format!("multisig_{n}_keys"),
            multisig(&[compressed(0)], 0b1, &keys, 1),
        );
    }
    // Weights that would overflow a u16 sum, behind the key-count check.
    let heavy: Vec<_> = (0..300u16)
        .map(|i| (public_key(0, &[(i % 251) as u8; 32]), 255))
        .collect();
    add(
        "multisig_300_heavy_keys",
        multisig(&[compressed(0)], 0b1, &heavy, 1),
    );
    let max_weights: Vec<_> = (0..10u8).map(|i| (pk(i + 1), 255)).collect();
    add(
        "multisig_max_weights",
        multisig(&[compressed(0)], 0b1, &max_weights, 2550),
    );
    add(
        "multisig_mixed_keys",
        multisig(
            &[compressed(1), compressed(2)],
            0b110,
            &[
                (public_key(0, &ed), 1),
                (public_key(1, &k1), 1),
                (public_key(2, &r1), 1),
                (public_key(6, &r1), 1),
            ],
            2,
        ),
    );
    let mut trailing = multisig(&[compressed(0)], 0b1, &two, 1);
    trailing.push(0);
    add("multisig_trailing", trailing);
    let ok = multisig(&[compressed(0)], 0b1, &two, 1);
    add("multisig_truncated", ok[..ok.len() - 1].to_vec());
    // A zkLogin key and signature in the new format: bytes unchecked here.
    add(
        "multisig_zklogin_parts",
        multisig(
            &[W::default().uleb(3).bytes(&[1, 2, 3])],
            0b1,
            &[(W::default().uleb(3).bytes(&[9; 20]), 1), (pk(2), 1)],
            1,
        ),
    );

    // Legacy: keys as base64 of flag || key, the bitmap as a roaring bitmap.
    let b64 = |flag: u8, key: &[u8]| {
        let mut bytes = vec![flag];
        bytes.extend_from_slice(key);
        <fastcrypto::encoding::Base64 as fastcrypto::encoding::Encoding>::encode(bytes)
    };
    let keys = vec![(b64(0, &ed), 1), (b64(1, &k1), 1), (b64(2, &r1), 1)];
    add(
        "legacy_ok",
        legacy(&[compressed(0)], &roaring(&[0]), &keys, 1),
    );
    add(
        "legacy_two",
        legacy(&[compressed(0), compressed(1)], &roaring(&[0, 1]), &keys, 2),
    );
    add(
        "legacy_index_9",
        legacy(&[compressed(0)], &roaring(&[9]), &keys, 1),
    );
    add(
        "legacy_index_10",
        legacy(&[compressed(0)], &roaring(&[10]), &keys, 1),
    );
    add(
        "legacy_empty_bitmap",
        legacy(&[compressed(0)], &roaring(&[]), &keys, 1),
    );
    let mut garbage = roaring(&[0]);
    garbage.extend_from_slice(&[1, 2, 3]);
    add(
        "legacy_bitmap_trailing",
        legacy(&[compressed(0)], &garbage, &keys, 1),
    );
    add(
        "legacy_bitmap_bad",
        legacy(&[compressed(0)], &[1, 2, 3], &keys, 1),
    );
    add(
        "legacy_passkey_key",
        legacy(&[compressed(0)], &roaring(&[0]), &[(b64(6, &r1), 1)], 1),
    );
    add(
        "legacy_zklogin_key",
        legacy(
            &[compressed(0)],
            &roaring(&[0]),
            &[(b64(5, &[9; 20]), 1)],
            1,
        ),
    );
    add(
        "legacy_invalid_point",
        legacy(
            &[compressed(0)],
            &roaring(&[0]),
            &[(b64(1, &[0xff; 33]), 1)],
            1,
        ),
    );
    add(
        "legacy_bad_base64",
        legacy(
            &[compressed(0)],
            &roaring(&[0]),
            &[("!!!".to_owned(), 1)],
            1,
        ),
    );
    add(
        "legacy_short_key",
        legacy(
            &[compressed(0)],
            &roaring(&[0]),
            &[(b64(0, &ed[..31]), 1)],
            1,
        ),
    );
    add(
        "legacy_duplicate_key",
        legacy(
            &[compressed(0)],
            &roaring(&[0]),
            &[(b64(0, &ed), 1), (b64(0, &ed), 1)],
            1,
        ),
    );
    add(
        "legacy_threshold_over",
        legacy(&[compressed(0)], &roaring(&[0]), &keys, 4),
    );
    add("legacy_no_sigs", legacy(&[], &roaring(&[0]), &keys, 1));
    let many: Vec<u32> = (0..151).collect();
    add(
        "legacy_bitmap_151",
        legacy(&[compressed(0)], &roaring(&many), &keys, 1),
    );

    // Passkey: authenticator data, client data JSON, and a Secp256r1
    // signature (flag, r || s, key) as a byte vector.
    let (_, r1_kp): (_, Secp256r1KeyPair) = get_key_pair_from_rng(&mut rng);
    let r1_sig = {
        use fastcrypto::traits::Signer;
        let s: fastcrypto::secp256r1::Secp256r1Signature = r1_kp.sign(b"message");
        s.as_bytes().to_vec()
    };
    let r1_pk = r1_kp.public().as_bytes().to_vec();
    let user_sig = |flag: u8, sig: &[u8], pk: &[u8]| {
        let mut b = vec![flag];
        b.extend_from_slice(sig);
        b.extend_from_slice(pk);
        b
    };
    let challenge = |n: usize| {
        <base64ct::Base64UrlUnpadded as base64ct::Encoding>::encode_string(&vec![7u8; n])
    };
    let client = |ty: &str, challenge: &str| {
        format!(r#"{{"type":"{ty}","challenge":"{challenge}","origin":"https://example.com"}}"#)
    };
    let passkey = |auth: &[u8], json: &[u8], sig: &[u8]| {
        W::default().raw(&[6]).bytes(auth).bytes(json).bytes(sig).0
    };
    let good_sig = user_sig(2, &r1_sig, &r1_pk);
    let good_json = client("webauthn.get", &challenge(32));
    add(
        "passkey_ok",
        passkey(&[1; 37], good_json.as_bytes(), &good_sig),
    );
    for (label, json) in [
        ("create", client("webauthn.create", &challenge(32))),
        ("payment", client("payment.get", &challenge(32))),
        ("unknown_type", client("webauthn.other", &challenge(32))),
        ("challenge_31", client("webauthn.get", &challenge(31))),
        ("challenge_33", client("webauthn.get", &challenge(33))),
        (
            "challenge_padded",
            client("webauthn.get", &format!("{}=", challenge(32))),
        ),
        (
            "challenge_bad_char",
            client("webauthn.get", &format!("{}!", &challenge(32)[..42])),
        ),
        (
            "no_origin",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{}"}}"#,
                challenge(32)
            ),
        ),
        (
            "no_challenge",
            r#"{"type":"webauthn.get","origin":"o"}"#.to_owned(),
        ),
        (
            "cross_origin",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{}","origin":"o","crossOrigin":true}}"#,
                challenge(32)
            ),
        ),
        (
            "cross_origin_string",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{}","origin":"o","crossOrigin":"yes"}}"#,
                challenge(32)
            ),
        ),
        (
            "extra_keys",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{}","origin":"o","x":[1,{{}}],"y":null}}"#,
                challenge(32)
            ),
        ),
        (
            "duplicate_challenge",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{0}","challenge":"{0}","origin":"o"}}"#,
                challenge(32)
            ),
        ),
        (
            "duplicate_extra",
            format!(
                r#"{{"type":"webauthn.get","challenge":"{}","origin":"o","x":1,"x":2}}"#,
                challenge(32)
            ),
        ),
        ("trailing_space", format!("{good_json} ")),
        ("trailing_garbage", format!("{good_json}x")),
        ("array", "[]".to_owned()),
        ("empty", String::new()),
    ] {
        add(
            &format!("passkey_json_{label}"),
            passkey(&[1; 37], json.as_bytes(), &good_sig),
        );
    }
    add(
        "passkey_json_not_utf8",
        passkey(&[1; 37], &[0xff, 0xfe], &good_sig),
    );
    add(
        "passkey_empty_auth_data",
        passkey(&[], good_json.as_bytes(), &good_sig),
    );
    add(
        "passkey_ed25519_sig",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(0, &r1_sig, &r1_pk[1..]),
        ),
    );
    add(
        "passkey_k1_sig",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(1, &r1_sig, &r1_pk),
        ),
    );
    add(
        "passkey_short_sig",
        passkey(&[1; 37], good_json.as_bytes(), &good_sig[..97]),
    );
    add(
        "passkey_bad_point",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(2, &r1_sig, &[0xff; 33]),
        ),
    );
    let mut compact = r1_pk.clone();
    compact[0] = 5;
    add(
        "passkey_compact_point",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(2, &r1_sig, &compact),
        ),
    );
    let mut zero_r = r1_sig.clone();
    zero_r[..32].fill(0);
    add(
        "passkey_zero_r",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(2, &zero_r, &r1_pk),
        ),
    );
    let mut zero_s = r1_sig.clone();
    zero_s[32..].fill(0);
    add(
        "passkey_zero_s",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(2, &zero_s, &r1_pk),
        ),
    );
    let mut big_s = r1_sig.clone();
    big_s[32..].fill(0xff);
    add(
        "passkey_s_over_order",
        passkey(&[1; 37], good_json.as_bytes(), &user_sig(2, &big_s, &r1_pk)),
    );
    let mut high_s = r1_sig.clone();
    {
        // n - s: the high-s twin of a valid signature.
        let n = hex_bytes("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
        let s = &r1_sig[32..];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = n[i] as i16 - s[i] as i16 - borrow;
            high_s[32 + i] = d.rem_euclid(256) as u8;
            borrow = i16::from(d < 0);
        }
    }
    add(
        "passkey_high_s",
        passkey(
            &[1; 37],
            good_json.as_bytes(),
            &user_sig(2, &high_s, &r1_pk),
        ),
    );
    let mut trailing = passkey(&[1; 37], good_json.as_bytes(), &good_sig);
    trailing.push(0);
    add("passkey_trailing", trailing);

    // A real single signature, for the length only.
    let (_, kp): (_, SuiKeyPair) = {
        let (a, k): (_, fastcrypto::ed25519::Ed25519KeyPair) = get_key_pair_from_rng(&mut rng);
        (a, SuiKeyPair::Ed25519(k))
    };
    let sig = sui_types::crypto::Signature::new_hashed(&[0; 32], &kp);
    add("real_ed25519", sig.as_ref().to_vec());

    let mut out = String::new();
    for (label, bytes) in cases {
        writeln!(out, "{label} {} {}", hex(&bytes), verdict(&bytes)).unwrap();
    }
    out
}
