// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A parsed message together with the two allocations it points into.
//!
//! This is the only module that erases a lifetime.

use std::fmt;

use crate::arena::{Alloc, Arena, Build, Measure};
use crate::base::Digest;
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

/// The bytes of one message as read off the wire. Never touched after
/// construction, so the heap allocation stays put while views point into it.
pub struct WireBuf(Vec<u8>);

impl From<Vec<u8>> for WireBuf {
    fn from(v: Vec<u8>) -> WireBuf {
        WireBuf(v)
    }
}

impl WireBuf {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for WireBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireBuf({} bytes)", self.0.len())
    }
}

/// A conversion between views over the same buffers. `apply` works for
/// every lifetime, so it cannot keep the view's references.
pub(crate) trait ViewMap<T: Wire, U: Wire> {
    fn apply(self, view: T::View<'_>) -> U::View<'_>;
}

/// A change to a view. `apply` works for every lifetime, so it cannot put
/// in references that do not live as long as the message.
pub(crate) trait ViewUpdate<T: Wire> {
    fn apply(self, view: &mut T::View<'_>);
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

/// The most arena bytes any one wire byte can cost: a three-byte
/// `MakeMoveVec(None, [])` becomes an 80-byte `Command`. Every parser
/// checks a sequence's length against the input left before reserving.
pub const MAX_ARENA_PER_WIRE_BYTE: usize = 32;

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
            n => (wire.0.len().saturating_mul(n) / 16).max(MIN_ARENA_GUESS),
        };
        let result = match Self::parse_guessed(&wire, guess) {
            Err(ParseError::ArenaFull) => Self::parse_measured(&wire),
            result => result,
        };
        Self::assemble(wire, result)
    }

    /// Like `parse`, but always two passes and an arena of exactly the size
    /// used. For messages that will be held for a long time.
    pub fn parse_exact(
        wire: impl Into<WireBuf>,
    ) -> std::result::Result<Self, (ParseError, WireBuf)> {
        let wire = wire.into();
        let result = Self::parse_measured(&wire);
        Self::assemble(wire, result)
    }

    fn assemble(
        wire: WireBuf,
        result: Result<(T::View<'static>, Arena, usize)>,
    ) -> std::result::Result<Self, (ParseError, WireBuf)> {
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

    /// One validating pass into an arena of `size` bytes.
    fn parse_guessed(wire: &WireBuf, size: usize) -> Result<(T::View<'static>, Arena, usize)> {
        // SAFETY: as in `parse_measured`.
        let bytes: &'static [u8] =
            unsafe { std::slice::from_raw_parts(wire.0.as_ptr(), wire.0.len()) };
        let mut arena = Arena::new(size)?;
        // SAFETY: as in `parse_measured`.
        let mut build = unsafe { Build::<'static>::new(&mut arena) };
        let mut r = Reader::new(bytes);
        let view = T::parse(&mut r, &mut build)?;
        r.finish()?;
        Ok((view, arena, build.used()))
    }

    fn parse_measured(wire: &WireBuf) -> Result<(T::View<'static>, Arena, usize)> {
        // SAFETY: the bytes live on the heap until the `WireBuf` is dropped,
        // which is after every use of the view: `get` ties the view's
        // lifetime to a borrow of the `Message` that owns the buffer, and on
        // the error path the view is discarded before the buffer is returned.
        let bytes: &'static [u8] =
            unsafe { std::slice::from_raw_parts(wire.0.as_ptr(), wire.0.len()) };

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

    /// Converts the view, keeping the buffers it points into.
    #[inline]
    pub(crate) fn map<U: Wire>(self, f: impl ViewMap<T, U>) -> Message<U> {
        Message {
            view: f.apply(self.view),
            wire: self.wire,
            arena: self.arena,
            arena_used: self.arena_used,
        }
    }

    /// Changes the view in place.
    #[inline]
    pub(crate) fn update(&mut self, f: impl ViewUpdate<T>) {
        f.apply(&mut self.view);
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

/// A view whose identity is a digest computed while parsing: the types the
/// reference implements `Message` for.
pub trait Digested {
    fn digest(&self) -> &Digest;
}

impl<T: Wire> Message<T>
where
    for<'a> T::View<'a>: Digested,
{
    pub fn digest(&self) -> &Digest {
        self.get().digest()
    }
}

// Equality, order and hash of a digested message are those of its digest.
impl<T: Wire> PartialEq for Message<T>
where
    for<'a> T::View<'a>: Digested,
{
    fn eq(&self, other: &Self) -> bool {
        self.digest() == other.digest()
    }
}

impl<T: Wire> Eq for Message<T> where for<'a> T::View<'a>: Digested {}

impl<T: Wire> PartialOrd for Message<T>
where
    for<'a> T::View<'a>: Digested,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Wire> Ord for Message<T>
where
    for<'a> T::View<'a>: Digested,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.digest().cmp(other.digest())
    }
}

impl<T: Wire> std::hash::Hash for Message<T>
where
    for<'a> T::View<'a>: Digested,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.digest().hash(state);
    }
}
