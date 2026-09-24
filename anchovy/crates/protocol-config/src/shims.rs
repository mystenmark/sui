// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Local stand-ins for what the reference imports from `move-core-types`,
//! `fastcrypto` and `mysten-common`, so that `lib.rs` stays line-for-line
//! comparable with `sui-protocol-config` without those dependencies.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `move_core_types::VARIANT_COUNT_MAX`.
pub const VARIANT_COUNT_MAX: u64 = 127;

/// The reference raises some limits under Antithesis and msim, neither of
/// which runs this code.
pub fn in_integration_test() -> bool {
    false
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s).as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".to_owned());
    }
    s.chunks(2)
        .map(|p| match (hex_nibble(p[0]), hex_nibble(p[1])) {
            (Some(hi), Some(lo)) => Ok(hi << 4 | lo),
            _ => Err("invalid hex digit".to_owned()),
        })
        .collect()
}

/// `fastcrypto::encoding::Hex`: accepts an optional `0x` prefix.
pub struct Hex;

impl Hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        hex_decode(s)
    }
}

/// `fastcrypto::encoding::Base58`.
pub struct Base58;

impl Base58 {
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        bs58::decode(s).into_vec().map_err(|e| e.to_string())
    }
}

/// `move_core_types::account_address::AccountAddress`, as far as protocol
/// config uses it: ordering, hex parsing, and its serde form (bare
/// lowercase hex when human-readable, the 32 bytes otherwise).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountAddress(pub [u8; 32]);

impl AccountAddress {
    pub const LENGTH: usize = 32;

    /// A `0x`-prefixed hex literal, left-padded with zeros when short.
    pub fn from_hex_literal(literal: &str) -> Result<Self, String> {
        let hex = literal
            .strip_prefix("0x")
            .ok_or_else(|| "missing 0x".to_owned())?;
        if hex.len() > Self::LENGTH * 2 {
            return Err("address too long".to_owned());
        }
        Self::from_hex(&format!("{hex:0>64}"))
    }

    fn from_hex(hex: &str) -> Result<Self, String> {
        let bytes = hex_decode(hex)?;
        Ok(AccountAddress(
            bytes.try_into().map_err(|_| "not 32 bytes".to_owned())?,
        ))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for AccountAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", self.to_hex())
    }
}

impl Serialize for AccountAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            self.to_hex().serialize(serializer)
        } else {
            serializer.serialize_newtype_struct("AccountAddress", &self.0)
        }
    }
}

impl<'de> Deserialize<'de> for AccountAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            AccountAddress::from_hex(&s).map_err(serde::de::Error::custom)
        } else {
            #[derive(Deserialize)]
            #[serde(rename = "AccountAddress")]
            struct Value([u8; 32]);
            Ok(AccountAddress(Value::deserialize(deserializer)?.0))
        }
    }
}
