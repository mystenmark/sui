// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A bump allocator for temporaries: one allocation up front, one free on
//! drop. Everything that would be a local variable if it did not need
//! dynamic storage is allocated from a [`Bump`].

use std::alloc::{self, Layout};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

/// Chunks are aligned to this, so any value with a smaller alignment can be
/// placed by bumping the offset.
const CHUNK_ALIGN: usize = 16;

struct Chunk {
    ptr: NonNull<u8>,
    size: usize,
}

impl Chunk {
    fn new(size: usize) -> Chunk {
        let layout = Layout::from_size_align(size, CHUNK_ALIGN).expect("chunk layout");
        // SAFETY: `size` is never zero.
        let ptr = unsafe { alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::handle_alloc_error(layout)
        };
        Chunk { ptr, size }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.size, CHUNK_ALIGN).expect("chunk layout");
        // SAFETY: allocated in `new` with this layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) };
    }
}

/// A bump allocator. Allocation bumps an offset; deallocation does
/// nothing; drop frees everything at once.
///
/// When the buffer runs out another chunk is allocated so that allocation
/// never fails, at the cost of a second free; [`Bump::chunks`] reports how
/// many there are so callers that promised one allocation can check.
pub struct Bump {
    /// The chunk being bumped: its base and size.
    current: Cell<(NonNull<u8>, usize)>,
    used: Cell<usize>,
    /// Every allocation ever made, chunk changes included.
    allocated: Cell<usize>,
    /// Bumped through `current` until it overflows; `reset` returns to it.
    first: Chunk,
    /// Chunks after the first, oldest first. An empty `Vec` allocates
    /// nothing, so an arena that never overflows is one allocation.
    extra: RefCell<Vec<Chunk>>,
}

impl Bump {
    /// An arena with `capacity` bytes in its first chunk.
    pub fn with_capacity(capacity: usize) -> Bump {
        let first = Chunk::new(capacity.max(CHUNK_ALIGN));
        Bump {
            current: Cell::new((first.ptr, first.size)),
            used: Cell::new(0),
            allocated: Cell::new(0),
            first,
            extra: RefCell::new(Vec::new()),
        }
    }

    /// How many chunks have been allocated. One means the initial capacity
    /// was enough.
    pub fn chunks(&self) -> usize {
        1 + self.extra.borrow().len()
    }

    /// Bytes handed out so far, alignment padding included, across chunks.
    pub fn allocated(&self) -> usize {
        self.allocated.get()
    }

    /// Bytes used in the current chunk.
    pub fn used(&self) -> usize {
        self.used.get()
    }

    /// Frees everything allocated, for reuse: overflow chunks are released
    /// and the first chunk is bumped from its start again.
    ///
    /// Taking `&mut self` is what makes this sound: every allocation borrows
    /// the arena, so none can be alive here.
    pub fn reset(&mut self) {
        self.extra.get_mut().clear();
        self.current.set((self.first.ptr, self.first.size));
        self.used.set(0);
        self.allocated.set(0);
    }

    #[inline]
    fn alloc(&self, layout: Layout) -> NonNull<[u8]> {
        assert!(layout.align() <= CHUNK_ALIGN, "over-aligned arena value");
        let start = self.used.get().next_multiple_of(layout.align());
        let end = start.saturating_add(layout.size());
        let (base, size) = self.current.get();
        if end > size {
            return self.alloc_slow(layout);
        }
        self.used.set(end);
        self.allocated.set(self.allocated.get() + (end - start));
        // SAFETY: `start + size <= chunk size`, so the range is inside the
        // chunk; the chunk outlives every allocation, being freed with `self`.
        let ptr = unsafe { NonNull::new_unchecked(base.as_ptr().add(start)) };
        NonNull::slice_from_raw_parts(ptr, layout.size())
    }

    /// A new chunk at least twice the last and large enough for the request.
    #[cold]
    fn alloc_slow(&self, layout: Layout) -> NonNull<[u8]> {
        let (_, last) = self.current.get();
        let chunk = Chunk::new(last.max(layout.size()).saturating_mul(2));
        self.current.set((chunk.ptr, chunk.size));
        self.used.set(0);
        self.extra.borrow_mut().push(chunk);
        self.alloc(layout)
    }
}

impl Default for Bump {
    /// A first chunk of 64 KiB.
    fn default() -> Bump {
        Bump::with_capacity(64 * 1024)
    }
}

// SAFETY: allocations are distinct, aligned, and live until the `Bump` is
// dropped; deallocation is a no-op, which the trait permits.
unsafe impl allocator_api2::alloc::Allocator for &Bump {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, allocator_api2::alloc::AllocError> {
        Ok(self.alloc(layout))
    }

    #[inline]
    unsafe fn deallocate(&self, _: NonNull<u8>, _: Layout) {}
}

// SAFETY: as above.
unsafe impl allocator_api2_04::alloc::Allocator for &Bump {
    #[inline]
    fn allocate(
        &self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, allocator_api2_04::alloc::AllocError> {
        Ok(self.alloc(layout))
    }

    #[inline]
    unsafe fn deallocate(&self, _: NonNull<u8>, _: Layout) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use allocator_api2::alloc::Allocator as _;

    #[test]
    fn bumps_with_alignment() {
        let bump = Bump::with_capacity(64);
        let a = (&bump)
            .allocate(Layout::from_size_align(3, 1).unwrap())
            .unwrap();
        let b = (&bump)
            .allocate(Layout::from_size_align(8, 8).unwrap())
            .unwrap();
        let a = a.as_ptr().cast::<u8>() as usize;
        let b = b.as_ptr().cast::<u8>() as usize;
        assert_eq!(b - a, 8);
        assert_eq!(bump.used(), 16);
        assert_eq!(bump.allocated(), 11);
        assert_eq!(bump.chunks(), 1);
    }

    #[test]
    fn grows_when_full() {
        let bump = Bump::with_capacity(32);
        for _ in 0..10 {
            (&bump)
                .allocate(Layout::from_size_align(16, 1).unwrap())
                .unwrap();
        }
        assert!(bump.chunks() > 1);
        assert_eq!(bump.allocated(), 160);
    }

    #[test]
    fn reset_reuses_the_first_chunk() {
        let mut bump = Bump::with_capacity(64);
        let first = (&bump)
            .allocate(Layout::from_size_align(8, 8).unwrap())
            .unwrap()
            .as_ptr()
            .cast::<u8>();
        (&bump)
            .allocate(Layout::from_size_align(1000, 1).unwrap())
            .unwrap();
        assert_eq!(bump.chunks(), 2);
        bump.reset();
        assert_eq!((bump.chunks(), bump.used(), bump.allocated()), (1, 0, 0));
        let again = (&bump)
            .allocate(Layout::from_size_align(8, 8).unwrap())
            .unwrap()
            .as_ptr()
            .cast::<u8>();
        assert_eq!(first, again);
    }

    #[test]
    fn oversized_request_gets_its_own_chunk() {
        let bump = Bump::with_capacity(16);
        let big = (&bump)
            .allocate(Layout::from_size_align(1000, 1).unwrap())
            .unwrap();
        assert_eq!(big.len(), 1000);
        assert_eq!(bump.chunks(), 2);
    }
}
