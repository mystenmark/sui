// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The single allocation that holds everything a parsed message does not
//! borrow from its wire bytes.
//!
//! Parsers are generic over [`Alloc`] and run twice: once with [`Measure`],
//! which only adds up sizes, and once with [`Build`], which writes into an
//! [`Arena`] of exactly the measured size. Both compute offsets with
//! [`bump`], so they agree as long as the parser makes the same calls, which
//! it does because it is the same code reading the same bytes.

use std::alloc::{self, Layout};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use crate::error::{ParseError, Result};

/// Every arena value must have an alignment that divides this.
pub const ARENA_ALIGN: usize = 16;

fn bump(off: &mut usize, layout: Layout) -> Result<usize> {
    debug_assert!(layout.align() <= ARENA_ALIGN);
    let start = off
        .checked_next_multiple_of(layout.align())
        .ok_or(ParseError::WireTooLarge)?;
    *off = start
        .checked_add(layout.size())
        .ok_or(ParseError::WireTooLarge)?;
    Ok(start)
}

pub trait Alloc<'a> {
    /// Whether values are kept. Code that reads back what it parsed, which
    /// the measure pass cannot do, runs only when this is true; it must
    /// still make the same reservations in both passes.
    const BUILD: bool;

    /// Reserves room for `n` values, to be filled in order.
    fn slice<T: Copy + 'a>(&mut self, n: usize) -> Result<SliceWriter<'a, T>>;

    /// Moves one value into the arena.
    fn value<T: Copy + 'a>(&mut self, v: T) -> Result<Ref<'a, T>> {
        let mut w = self.slice(1)?;
        w.push(v);
        Ok(Ref(w.finish().first()))
    }
}

/// A reference to one arena value.
///
/// The measure pass has nowhere to put the value, so there it holds nothing
/// and dereferencing panics. Parsers never read back what they built, and
/// only build-pass output leaves the crate.
#[derive(Debug)]
pub struct Ref<'a, T>(Option<&'a T>);

impl<T> Clone for Ref<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Ref<'_, T> {}

impl<T> std::ops::Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.expect("measure-pass value dereferenced")
    }
}

impl<T: PartialEq> PartialEq for Ref<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Ref<'_, T> {}

/// A reserved, partly filled slice. Dropping it without `finish` leaks
/// nothing, since arena values are `Copy`.
pub struct SliceWriter<'a, T> {
    // Null in the measure pass.
    ptr: *mut T,
    len: usize,
    cap: usize,
    _arena: PhantomData<&'a mut [T]>,
}

impl<'a, T: Copy + 'a> SliceWriter<'a, T> {
    #[inline]
    pub fn push(&mut self, v: T) {
        if self.ptr.is_null() {
            return;
        }
        assert!(self.len < self.cap, "arena slice overfilled");
        // SAFETY: `ptr` is valid for `cap` writes of `T` and `len < cap`.
        unsafe { self.ptr.add(self.len).write(v) };
        self.len += 1;
    }

    /// Pushes `v` unless it equals the value pushed last. Cheap relief for
    /// `sort_dedup` when repeats tend to be adjacent.
    #[inline]
    pub fn push_unless_repeat(&mut self, v: T)
    where
        T: PartialEq,
    {
        if !self.ptr.is_null() && self.len > 0 {
            // SAFETY: slot `len - 1` was written by `push`.
            if unsafe { *self.ptr.add(self.len - 1) } == v {
                return;
            }
        }
        self.push(v);
    }

    /// Sorts what has been pushed and drops repeats. The reservation keeps
    /// its size, so the space the repeats took is left unused.
    pub fn sort_dedup(&mut self)
    where
        T: Ord,
    {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: the first `len` slots are initialized and `self` is the
        // only handle to them until `finish`.
        let filled = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) };
        filled.sort_unstable();
        let mut kept = 0;
        for i in 0..filled.len() {
            if kept == 0 || filled[kept - 1] != filled[i] {
                filled[kept] = filled[i];
                kept += 1;
            }
        }
        self.len = kept;
    }

    /// The filled prefix. Empty in the measure pass.
    pub fn finish(self) -> &'a [T] {
        if self.ptr.is_null() {
            return &[];
        }
        // SAFETY: the first `len` slots were written by `push`, the memory
        // lives for `'a`, and nothing else points into this reservation.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[derive(Default)]
pub struct Measure {
    off: usize,
}

impl Measure {
    pub fn size(&self) -> usize {
        self.off
    }
}

impl<'a> Alloc<'a> for Measure {
    const BUILD: bool = false;

    fn slice<T: Copy + 'a>(&mut self, n: usize) -> Result<SliceWriter<'a, T>> {
        let layout = Layout::array::<T>(n).map_err(|_| ParseError::WireTooLarge)?;
        bump(&mut self.off, layout)?;
        Ok(SliceWriter {
            ptr: ptr::null_mut(),
            len: 0,
            cap: n,
            _arena: PhantomData,
        })
    }
}

pub struct Arena {
    ptr: NonNull<u8>,
    size: usize,
}

// SAFETY: an `Arena` owns its allocation and hands out no interior mutability.
unsafe impl Send for Arena {}
// SAFETY: as above.
unsafe impl Sync for Arena {}

impl Arena {
    /// One allocation of exactly `size` bytes, or none if `size` is zero.
    pub fn new(size: usize) -> Result<Arena> {
        if size == 0 {
            return Ok(Arena {
                ptr: NonNull::dangling(),
                size,
            });
        }
        let layout =
            Layout::from_size_align(size, ARENA_ALIGN).map_err(|_| ParseError::WireTooLarge)?;
        // SAFETY: `layout` has non-zero size.
        let ptr = unsafe { alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::handle_alloc_error(layout)
        };
        Ok(Arena { ptr, size })
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        if self.size != 0 {
            let layout = Layout::from_size_align(self.size, ARENA_ALIGN).expect("checked in new");
            // SAFETY: allocated in `new` with this layout.
            unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

/// Writes into an [`Arena`]. `'a` is a lifetime the caller vouches the arena
/// outlives; see `Message`.
pub struct Build<'a> {
    base: NonNull<u8>,
    size: usize,
    off: usize,
    _arena: PhantomData<&'a mut [u8]>,
}

impl<'a> Build<'a> {
    /// # Safety
    /// The arena's allocation must live, unmoved and otherwise unused, for `'a`.
    pub unsafe fn new(arena: &mut Arena) -> Build<'a> {
        Build {
            base: arena.ptr,
            size: arena.size,
            off: 0,
            _arena: PhantomData,
        }
    }

    pub fn used(&self) -> usize {
        self.off
    }
}

impl<'a> Alloc<'a> for Build<'a> {
    const BUILD: bool = true;

    fn slice<T: Copy + 'a>(&mut self, n: usize) -> Result<SliceWriter<'a, T>> {
        let layout = Layout::array::<T>(n).map_err(|_| ParseError::WireTooLarge)?;
        let mut off = self.off;
        let start = bump(&mut off, layout)?;
        if off > self.size {
            return Err(ParseError::ArenaMismatch);
        }
        self.off = off;
        let ptr = if layout.size() == 0 {
            // An empty arena's base is not aligned for `T`.
            NonNull::<T>::dangling().as_ptr()
        } else {
            // SAFETY: `start..off` is inside the allocation. `base` is aligned to
            // `ARENA_ALIGN` and `start` to `align_of::<T>()`, which divides it.
            unsafe { self.base.as_ptr().add(start) }.cast::<T>()
        };
        Ok(SliceWriter {
            ptr,
            len: 0,
            cap: n,
            _arena: PhantomData,
        })
    }
}
