// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator API's BCS requests against sui-types' encodings, from
//! `sui-oracle --grpc-requests`.

use messages::fast::{Bump, Writer};
use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, SystemStateRequest,
    TransactionInfoRequest,
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Parses `bytes` as `ty` and writes it back.
fn roundtrip(ty: &str, bytes: &[u8]) -> messages::Result<Vec<u8>> {
    let bump = Bump::default();
    let mut w = Writer::new_in(&bump, bytes.len());
    match ty {
        "ObjectInfoRequest" => ObjectInfoRequest::parse(bytes)?.write(&mut w),
        "TransactionInfoRequest" => TransactionInfoRequest::parse(bytes)?.write(&mut w),
        "CheckpointRequest" => CheckpointRequest::parse(bytes)?.write(&mut w),
        "CheckpointRequestV2" => CheckpointRequestV2::parse(bytes)?.write(&mut w),
        "SystemStateRequest" => SystemStateRequest::parse(bytes)?.write(&mut w),
        _ => panic!("unknown request type {ty}"),
    }
    Ok(w.finish_bytes().to_vec())
}

#[test]
fn requests_match_the_reference() {
    let vectors = include_str!("data/grpc_requests.txt");
    let mut count = 0;
    for line in vectors.lines() {
        let (ty, hex) = line.split_once(' ').unwrap();
        let bytes = unhex(hex);
        assert_eq!(roundtrip(ty, &bytes).unwrap(), bytes, "{line}");
        for len in 0..bytes.len() {
            assert!(roundtrip(ty, &bytes[..len]).is_err(), "{line} cut to {len}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(roundtrip(ty, &long).is_err(), "{line} with a trailing byte");
        count += 1;
    }
    assert_eq!(count, 23);
}

#[test]
fn unknown_variants_are_rejected() {
    let mut bytes = unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f0000");
    bytes[32] = 2;
    assert!(ObjectInfoRequest::parse(&bytes).is_err());
    bytes[32] = 0;
    bytes[33] = 2;
    assert!(ObjectInfoRequest::parse(&bytes).is_err());
    assert!(CheckpointRequest::parse(&[2, 0]).is_err());
    assert!(SystemStateRequest::parse(&[2]).is_err());
}
