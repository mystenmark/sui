// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Fixed-layout leaf types. All are wire records: a `&T` points straight
//! into the wire buffer.

use std::fmt;

use crate::error::{ParseError, Result};
use crate::reader::{Reader, WireRecord};

fn fmt_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("0x")?;
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

macro_rules! bytes32 {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name(pub [u8; 32]);

        // SAFETY: a transparent wrapper of a byte array.
        unsafe impl WireRecord for $name {}

        impl $name {
            pub const LENGTH: usize = 32;

            pub fn parse<'a>(r: &mut Reader<'a>) -> Result<&'a $name> {
                r.record()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt_hex(&self.0, f)
            }
        }
    };
}

bytes32!(
    /// A Move `AccountAddress`.
    AccountAddress
);
bytes32!(SuiAddress);
bytes32!(ObjectId);

impl ObjectId {
    /// The id whose last bytes are the big-endian `n`, as for system objects.
    pub const fn from_u16(n: u16) -> ObjectId {
        let mut bytes = [0; 32];
        bytes[30] = (n >> 8) as u8;
        bytes[31] = n as u8;
        ObjectId(bytes)
    }
}

/// A little-endian `u64` with alignment 1.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct U64Le(pub [u8; 8]);

// SAFETY: a transparent wrapper of a byte array.
unsafe impl WireRecord for U64Le {}

impl U64Le {
    pub const fn new(v: u64) -> U64Le {
        U64Le(v.to_le_bytes())
    }

    pub const fn get(self) -> u64 {
        u64::from_le_bytes(self.0)
    }
}

impl fmt::Debug for U64Le {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// An object version.
pub type SequenceNumber = u64;

/// A 32-byte digest as it sits on the wire: a length byte, always 32, then
/// the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Digest {
    len: u8,
    pub bytes: [u8; 32],
}

// SAFETY: `repr(C)` over byte fields: alignment 1, no padding. `len` is
// checked wherever a `&Digest` is produced but no value of it is invalid.
unsafe impl WireRecord for Digest {}

impl Digest {
    pub const fn new(bytes: [u8; 32]) -> Digest {
        Digest { len: 32, bytes }
    }

    pub fn parse<'a>(r: &mut Reader<'a>) -> Result<&'a Digest> {
        let d: &Digest = r.record()?;
        d.check()?;
        Ok(d)
    }

    fn check(&self) -> Result<()> {
        if self.len == 32 {
            Ok(())
        } else {
            // A length of 128 or more is a multi-byte uleb128 and also wrong.
            Err(ParseError::WrongLength {
                ty: "Digest",
                expected: 32,
                actual: u32::from(self.len),
            })
        }
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hex(&self.bytes, f)
    }
}

pub type ObjectDigest = Digest;
pub type TransactionDigest = Digest;
pub type TransactionEffectsDigest = Digest;
pub type TransactionEventsDigest = Digest;
pub type EffectsAuxDataDigest = Digest;
pub type CheckpointDigest = Digest;
pub type CheckpointContentsDigest = Digest;
pub type CheckpointArtifactsDigest = Digest;
pub type ConsensusCommitDigest = Digest;
pub type AdditionalConsensusStateDigest = Digest;
pub type ChainIdentifier = Digest;

/// `(ObjectID, SequenceNumber, ObjectDigest)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct ObjectRef {
    pub id: ObjectId,
    pub version: U64Le,
    pub digest: ObjectDigest,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for ObjectRef {}

impl ObjectRef {
    pub fn parse<'a>(r: &mut Reader<'a>) -> Result<&'a ObjectRef> {
        let o: &ObjectRef = r.record()?;
        o.digest.check()?;
        Ok(o)
    }

    pub fn parse_vec<'a>(r: &mut Reader<'a>) -> Result<&'a [ObjectRef]> {
        let refs: &[ObjectRef] = r.record_vec()?;
        for o in refs {
            o.digest.check()?;
        }
        Ok(refs)
    }
}

/// `(ObjectID, SequenceNumber)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct ObjectKey {
    pub id: ObjectId,
    pub version: U64Le,
}

// SAFETY: `repr(C)` over wire records: alignment 1, no padding.
unsafe impl WireRecord for ObjectKey {}

const _: () = {
    assert!(size_of::<Digest>() == 33 && align_of::<Digest>() == 1);
    assert!(size_of::<ObjectRef>() == 73 && align_of::<ObjectRef>() == 1);
    assert!(size_of::<ObjectKey>() == 40 && align_of::<ObjectKey>() == 1);
};
