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
pub unsafe trait Wire: 'static {
    type View<'a>: Copy + 'a;

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
}

// SAFETY: a `Message` is immutable after construction and owns what `view`
// points to, so it is as thread-safe as the view's contents.
unsafe impl<T: Wire> Send for Message<T> where for<'a> T::View<'a>: Sync {}
// SAFETY: as above.
unsafe impl<T: Wire> Sync for Message<T> where for<'a> T::View<'a>: Sync {}

impl<T: Wire> Message<T> {
    /// Parses `wire` as exactly one `T`. On failure the buffer is handed back.
    pub fn parse(wire: impl Into<WireBuf>) -> std::result::Result<Self, (ParseError, WireBuf)> {
        let wire = wire.into();
        match Self::parse_inner(&wire) {
            Ok((view, arena)) => Ok(Message { view, wire, arena }),
            Err(e) => Err((e, wire)),
        }
    }

    fn parse_inner(wire: &WireBuf) -> Result<(T::View<'static>, Arena)> {
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
        let mut r = Reader::new(bytes);
        let view = T::parse(&mut r, &mut build)?;
        r.finish()?;
        if build.used() != arena.size() {
            return Err(ParseError::ArenaMismatch);
        }
        Ok((view, arena))
    }

    pub fn get(&self) -> &T::View<'_> {
        T::shrink(&self.view)
    }

    pub fn wire_bytes(&self) -> &[u8] {
        self.wire.as_slice()
    }

    pub fn arena_size(&self) -> usize {
        self.arena.size()
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
