// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

pub type Result<T> = std::result::Result<T, ParseError>;

/// Why a byte string is not a well-formed encoding of the requested type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    UnexpectedEof,
    /// A uleb128 with a zero most-significant digit.
    NonCanonicalUleb128,
    /// A uleb128 above `u32::MAX`.
    Uleb128Overflow,
    SequenceTooLong,
    ContainerDepthExceeded,
    InvalidBool,
    InvalidOptionTag,
    UnknownVariant {
        ty: &'static str,
        tag: u32,
    },
    InvalidUtf8,
    /// Map keys not strictly increasing by serialized bytes.
    NonCanonicalMap,
    TrailingBytes,
    /// A byte string that must have one fixed length.
    WrongLength {
        ty: &'static str,
        expected: u32,
        actual: u32,
    },
    /// `SenderSignedData` with other than one transaction.
    NotOneTransaction,
    WireTooLarge,
    /// A single-pass build outgrew its guessed arena; the caller retries
    /// with a measured one. Never returned from `Message::parse`.
    ArenaFull,
    /// The build pass did not allocate what the measure pass counted. Always a bug.
    ArenaMismatch,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ParseError {}
