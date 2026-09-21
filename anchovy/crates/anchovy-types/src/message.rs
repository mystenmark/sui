// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A parsed message together with the two allocations it points into.
//!
//! This is the only module that erases a lifetime.

use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use crate::arena::{Alloc, Arena, Build, Measure};
use crate::error::{ParseError, Result};
use crate::reader::Reader;

/// A type that can be parsed from BCS into a borrowed view.
///
/// # Safety
/// `shrink` must be implemented as `v`, which the compiler accepts only if
/// `View` is covariant in its lifetime. [`Message`] relies on that.
/// Everything `parse` puts in the arena must have an alignment of at most
/// `ARENA_ALIGN`; `Alloc::slice` refuses larger ones at compile time.
pub unsafe trait Wire: 'static {
    type View<'a>: Copy + 'a;

    /// How much arena a single-pass parse reserves, in sixteenths of the
    /// wire size. Picked per type from mainnet data so that about 99% of
    /// messages fit; the rest are parsed again with a measured arena.
    const ARENA_GUESS_SIXTEENTHS: usize;

    fn parse<'a, A: Alloc<'a>>(r: &mut Reader<'a>, a: &mut A) -> Result<Self::View<'a>>;

    fn shrink<'l, 's: 'l>(v: &'l Self::View<'s>) -> &'l Self::View<'l>;
}

/// The bytes of one message as read off the wire: one allocation, never
/// reallocated or written after construction.
pub struct WireBuf {
    // The raw parts of a `Vec<u8>`. Held apart so that moving a `WireBuf`
    // asserts nothing about the heap bytes that views point into.
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
}

// SAFETY: a `WireBuf` owns its bytes and never mutates them.
unsafe impl Send for WireBuf {}
// SAFETY: as above.
unsafe impl Sync for WireBuf {}

impl From<Vec<u8>> for WireBuf {
    fn from(v: Vec<u8>) -> WireBuf {
        let mut v = ManuallyDrop::new(v);
        WireBuf {
            // SAFETY: a `Vec`'s pointer is never null.
            ptr: unsafe { NonNull::new_unchecked(v.as_mut_ptr()) },
            len: v.len(),
            cap: v.capacity(),
        }
    }
}

impl WireBuf {
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialized bytes while `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn into_vec(self) -> Vec<u8> {
        let this = ManuallyDrop::new(self);
        // SAFETY: these are the parts of the `Vec` this was made from.
        unsafe { Vec::from_raw_parts(this.ptr.as_ptr(), this.len, this.cap) }
    }
}

impl fmt::Debug for WireBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireBuf({} bytes)", self.len)
    }
}

impl Drop for WireBuf {
    fn drop(&mut self) {
        // SAFETY: these are the parts of the `Vec` this was made from.
        drop(unsafe { Vec::from_raw_parts(self.ptr.as_ptr(), self.len, self.cap) });
    }
}

/// A parsed `T`. Dropping it frees the wire buffer and the arena and nothing
/// else.
pub struct Message<T: Wire> {
    // Points into `wire` and `arena`; `'static` stands for "while self lives".
    view: T::View<'static>,
    wire: WireBuf,
    arena: Arena,
    arena_used: usize,
}

/// The smallest single-pass guess, so that tiny messages with a fixed
/// overhead do not fall back.
pub const MIN_ARENA_GUESS: usize = 256;

// SAFETY: a `Message` is immutable after construction and owns what `view`
// points to, so it is as thread-safe as the view's contents.
unsafe impl<T: Wire> Send for Message<T> where for<'a> T::View<'a>: Send {}
// SAFETY: as above.
unsafe impl<T: Wire> Sync for Message<T> where for<'a> T::View<'a>: Sync {}

impl<T: Wire> Message<T> {
    /// Parses `wire` as exactly one `T`. On failure the buffer is handed back.
    ///
    /// One pass into an arena guessed from the wire size; if the guess is
    /// too small, the exact two-pass parse runs instead. The arena may
    /// therefore be larger than what is used.
    pub fn parse(wire: impl Into<WireBuf>) -> std::result::Result<Self, (ParseError, WireBuf)> {
        let wire = wire.into();
        let guess = match T::ARENA_GUESS_SIXTEENTHS {
            0 => 0,
            n => (wire.len.saturating_mul(n) / 16).max(MIN_ARENA_GUESS),
        };
        let result = match Self::parse_guessed(&wire, guess) {
            Err(ParseError::ArenaFull) => Self::parse_measured(&wire),
            result => result,
        };
        match result {
            Ok((view, arena, arena_used)) => Ok(Message {
                view,
                wire,
                arena,
                arena_used,
            }),
            Err(e) => Err((e, wire)),
        }
    }

    /// Like `parse`, but always two passes and an arena of exactly the size
    /// used. For messages that will be held for a long time.
    pub fn parse_exact(
        wire: impl Into<WireBuf>,
    ) -> std::result::Result<Self, (ParseError, WireBuf)> {
        let wire = wire.into();
        match Self::parse_measured(&wire) {
            Ok((view, arena, arena_used)) => Ok(Message {
                view,
                wire,
                arena,
                arena_used,
            }),
            Err(e) => Err((e, wire)),
        }
    }

    /// One validating pass into an arena of `size` bytes.
    fn parse_guessed(wire: &WireBuf, size: usize) -> Result<(T::View<'static>, Arena, usize)> {
        // SAFETY: as in `parse_measured`.
        let bytes: &'static [u8] =
            unsafe { std::slice::from_raw_parts(wire.ptr.as_ptr(), wire.len) };
        let mut arena = Arena::new(size)?;
        // SAFETY: as in `parse_measured`.
        let mut build = unsafe { Build::<'static>::new(&mut arena) };
        let mut r = Reader::new(bytes);
        let view = T::parse(&mut r, &mut build)?;
        r.finish()?;
        Ok((view, arena, build.used()))
    }

    /// The arena size parsing `bytes` would need, without allocating it.
    pub fn measure(bytes: &[u8]) -> Result<usize> {
        let mut measure = Measure::default();
        let mut r = Reader::new(bytes);
        T::parse(&mut r, &mut measure)?;
        r.finish()?;
        Ok(measure.size())
    }

    fn parse_measured(wire: &WireBuf) -> Result<(T::View<'static>, Arena, usize)> {
        // SAFETY: the bytes live on the heap until the `WireBuf` is dropped,
        // which is after every use of the view: `get` ties the view's
        // lifetime to a borrow of the `Message` that owns the buffer, and on
        // the error path the view is discarded before the buffer is returned.
        let bytes: &'static [u8] =
            unsafe { std::slice::from_raw_parts(wire.ptr.as_ptr(), wire.len) };

        // Step 1: measure.
        let mut measure = Measure::default();
        let mut r = Reader::new(bytes);
        T::parse(&mut r, &mut measure)?;
        r.finish()?;

        // Step 2: the one allocation.
        let mut arena = Arena::new(measure.size())?;

        // Step 3: build.
        // SAFETY: the arena is returned alongside the view and kept in the
        // same `Message`; its allocation does not move when the `Arena` does.
        let mut build = unsafe { Build::<'static>::new(&mut arena) };
        // SAFETY: step 1 drove `T::parse` to success over these bytes.
        let mut r = unsafe { Reader::revisit(bytes) };
        let view = T::parse(&mut r, &mut build)?;
        r.finish()?;
        if build.used() != arena.size() {
            return Err(ParseError::ArenaMismatch);
        }
        Ok((view, arena, build.used()))
    }

    pub fn get(&self) -> &T::View<'_> {
        T::shrink(&self.view)
    }

    pub fn wire_bytes(&self) -> &[u8] {
        self.wire.as_slice()
    }

    /// The arena allocation. After `parse` it may exceed `arena_used`.
    pub fn arena_size(&self) -> usize {
        self.arena.size()
    }

    /// The arena bytes the view points into.
    pub fn arena_used(&self) -> usize {
        self.arena_used
    }
}

impl<T: Wire> fmt::Debug for Message<T>
where
    for<'a> T::View<'a>: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}
