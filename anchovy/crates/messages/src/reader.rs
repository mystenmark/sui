// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A cursor over BCS bytes. Accepts exactly what `bcs` 0.1.6 accepts at the
//! encoding level: minimal uleb128, bounded lengths and depth, strict tags.

use crate::error::{ParseError, Result};

pub const MAX_SEQUENCE_LENGTH: usize = (1 << 31) - 1;
pub const MAX_CONTAINER_DEPTH: u32 = 500;

/// A type whose in-memory layout is its wire layout.
///
/// # Safety
/// Alignment 1, no padding, and every bit pattern is a valid value.
pub unsafe trait WireRecord: Copy + 'static {}

// SAFETY: byte arrays have alignment 1, no padding, and no invalid values.
unsafe impl<const N: usize> WireRecord for [u8; N] {}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: u32,
    struct_tags: usize,
    // Whether `str` may skip UTF-8 validation.
    strs_validated: bool,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader {
            buf,
            pos: 0,
            depth: 0,
            struct_tags: 0,
            strs_validated: false,
        }
    }

    /// A reader for a second pass over bytes that a first pass accepted.
    ///
    /// # Safety
    /// A reader made with `new` over these same bytes must have been driven
    /// to success by the same parser this one will be given. That parser
    /// then asks for the same strings at the same positions, each already
    /// validated as UTF-8.
    pub unsafe fn revisit(buf: &'a [u8]) -> Self {
        Reader {
            strs_validated: true,
            ..Reader::new(buf)
        }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The bytes consumed since `start`, a value earlier returned by `pos`.
    #[inline]
    pub fn span(&self, start: usize) -> &'a [u8] {
        &self.buf[start..self.pos]
    }

    pub fn finish(&self) -> Result<()> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ParseError::TrailingBytes)
        }
    }

    /// `bcs` counts every struct, newtype struct, tuple struct and enum
    /// toward the depth limit; tuples, sequences, maps and options are free.
    /// A failed parse is abandoned, so error paths need not call `leave`.
    ///
    /// Only type tags recurse, so the count matters only on a path from a
    /// root to a `TypeTag`, and every container on such a path is counted.
    /// Elsewhere it makes no difference; the hot leaves `Argument` and
    /// `ObjectArg` skip it, other parsers count where `bcs` would.
    #[inline]
    pub fn enter(&mut self) -> Result<()> {
        if self.depth == MAX_CONTAINER_DEPTH {
            return Err(ParseError::ContainerDepthExceeded);
        }
        self.depth += 1;
        Ok(())
    }

    #[inline]
    pub fn leave(&mut self) {
        self.depth -= 1;
    }

    /// How many struct tags have been read. Known in both passes, unlike the
    /// parsed tags, so the transaction index sizes its package list from it.
    pub fn struct_tags(&self) -> usize {
        self.struct_tags
    }

    pub(crate) fn count_struct_tag(&mut self) {
        self.struct_tags += 1;
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(ParseError::UnexpectedEof);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<&'a [u8; N]> {
        let out = self.bytes(N)?;
        Ok(out.try_into().expect("length checked"))
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(ParseError::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(*self.array()?))
    }

    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(*self.array()?))
    }

    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(*self.array()?))
    }

    #[inline]
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ParseError::InvalidBool),
        }
    }

    /// Whether an `Option` is `Some`.
    #[inline]
    pub fn option(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ParseError::InvalidOptionTag),
        }
    }

    #[inline]
    pub fn option_u64(&mut self) -> Result<Option<u64>> {
        Ok(if self.option()? {
            Some(self.u64()?)
        } else {
            None
        })
    }

    #[inline]
    pub fn uleb128(&mut self) -> Result<u32> {
        let first = self.u8()?;
        if first < 0x80 {
            return Ok(u32::from(first));
        }
        self.uleb128_multibyte(first)
    }

    #[cold]
    fn uleb128_multibyte(&mut self, first: u8) -> Result<u32> {
        let mut value = u64::from(first & 0x7f);
        let mut shift = 7;
        while shift < 35 {
            let byte = self.u8()?;
            let digit = byte & 0x7f;
            value |= u64::from(digit) << shift;
            if digit == byte {
                if digit == 0 {
                    return Err(ParseError::NonCanonicalUleb128);
                }
                return u32::try_from(value).map_err(|_| ParseError::Uleb128Overflow);
            }
            shift += 7;
        }
        Err(ParseError::Uleb128Overflow)
    }

    /// An enum variant index.
    #[inline]
    pub fn variant(&mut self) -> Result<u32> {
        self.uleb128()
    }

    /// A sequence or map length.
    #[inline]
    pub fn length(&mut self) -> Result<usize> {
        let len = self.uleb128()? as usize;
        if len > MAX_SEQUENCE_LENGTH {
            return Err(ParseError::SequenceTooLong);
        }
        Ok(len)
    }

    /// A sequence length, for elements that occupy at least `min_wire_size`
    /// bytes each. Rejecting a length the input cannot hold is what bounds
    /// arena size by input size.
    #[inline]
    pub fn seq_len(&mut self, min_wire_size: usize) -> Result<usize> {
        debug_assert!(min_wire_size > 0);
        let len = self.length()?;
        if len > self.remaining() / min_wire_size {
            return Err(ParseError::UnexpectedEof);
        }
        Ok(len)
    }

    /// A length-prefixed byte string.
    #[inline]
    pub fn byte_vec(&mut self) -> Result<&'a [u8]> {
        let len = self.length()?;
        self.bytes(len)
    }

    #[inline]
    pub fn str(&mut self) -> Result<&'a str> {
        let bytes = self.byte_vec()?;
        // Nearly every string is a short ASCII identifier, for which the
        // general validator's setup costs more than the check.
        if self.strs_validated || bytes.is_ascii() {
            debug_assert!(std::str::from_utf8(bytes).is_ok());
            // SAFETY: ASCII is UTF-8; otherwise see `revisit`.
            return Ok(unsafe { std::str::from_utf8_unchecked(bytes) });
        }
        std::str::from_utf8(bytes).map_err(|_| ParseError::InvalidUtf8)
    }

    /// `n` consecutive fixed-layout records, viewed in place.
    #[inline]
    pub fn records<W: WireRecord>(&mut self, n: usize) -> Result<&'a [W]> {
        let size = n
            .checked_mul(size_of::<W>())
            .ok_or(ParseError::UnexpectedEof)?;
        let bytes = self.bytes(size)?;
        // SAFETY: `bytes` is exactly `n * size_of::<W>()` initialized bytes, and
        // `WireRecord` promises alignment 1 and no invalid bit patterns.
        Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<W>(), n) })
    }

    #[inline]
    pub fn record<W: WireRecord>(&mut self) -> Result<&'a W> {
        Ok(&self.records::<W>(1)?[0])
    }

    /// A length-prefixed sequence of fixed-layout records.
    #[inline]
    pub fn record_vec<W: WireRecord>(&mut self) -> Result<&'a [W]> {
        let len = self.length()?;
        self.records(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uleb(bytes: &[u8]) -> Result<u32> {
        let mut r = Reader::new(bytes);
        let v = r.uleb128()?;
        r.finish()?;
        Ok(v)
    }

    #[test]
    fn uleb128_values() {
        assert_eq!(uleb(&[0]), Ok(0));
        assert_eq!(uleb(&[0x7f]), Ok(127));
        assert_eq!(uleb(&[0x80, 0x01]), Ok(128));
        assert_eq!(uleb(&[0xff, 0xff, 0xff, 0xff, 0x0f]), Ok(u32::MAX));
    }

    #[test]
    fn uleb128_rejects() {
        assert_eq!(uleb(&[0x80, 0x00]), Err(ParseError::NonCanonicalUleb128));
        assert_eq!(
            uleb(&[0x80, 0x80, 0x00]),
            Err(ParseError::NonCanonicalUleb128)
        );
        assert_eq!(
            uleb(&[0xff, 0xff, 0xff, 0xff, 0x10]),
            Err(ParseError::Uleb128Overflow)
        );
        assert_eq!(
            uleb(&[0xff, 0xff, 0xff, 0xff, 0x8f, 0x01]),
            Err(ParseError::Uleb128Overflow)
        );
        assert_eq!(uleb(&[0x80]), Err(ParseError::UnexpectedEof));
    }

    #[test]
    fn seq_len_is_bounded_by_input() {
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x07, 0, 0]);
        assert_eq!(r.seq_len(1), Err(ParseError::UnexpectedEof));
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x08]);
        assert_eq!(r.length(), Err(ParseError::SequenceTooLong));
        let mut r = Reader::new(&[2, 0, 0, 0]);
        assert_eq!(r.seq_len(2), Err(ParseError::UnexpectedEof));
        let mut r = Reader::new(&[2, 0, 0, 0, 0]);
        assert_eq!(r.seq_len(2), Ok(2));
    }

    #[test]
    fn depth_limit() {
        let mut r = Reader::new(&[]);
        for _ in 0..MAX_CONTAINER_DEPTH {
            r.enter().unwrap();
        }
        assert_eq!(r.enter(), Err(ParseError::ContainerDepthExceeded));
    }
}
