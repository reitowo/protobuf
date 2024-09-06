// Protocol Buffers - Google's data interchange format
// Copyright 2024 Google LLC.  All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file or at
// https://developers.google.com/open-source/licenses/bsd

use super::sys::arena::{
    upb_Arena_Free, upb_Arena_Fuse, upb_Arena_Malloc, upb_Arena_New, RawArena, UPB_MALLOC_ALIGN,
};

use std::alloc::{self, Layout};
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr;
use std::slice;

/// A wrapper over a `upb_Arena`.
///
/// This is not a safe wrapper per se, because the allocation functions still
/// have sharp edges (see their safety docs for more info).
///
/// This is an owning type and will automatically free the arena when
/// dropped.
///
/// Note that this type is not `Sync` as it implements unsynchronized interior
/// mutability. The upb_Arena C object could be understood as being Sync (at
/// least vacuously under current API since there are not any const upb_Arena*
/// API functions), but the Rust Arena is necessarily expressed as interior
/// mutability (&self rather than &mut self receivers) See https://doc.rust-lang.org/nomicon/lifetime-mismatch.html and
/// https://blog.reverberate.org/2021/12/19/arenas-and-rust.html, and the
/// 'known problems' section of https://rust-lang.github.io/rust-clippy/master/index.html#/mut_from_ref.
#[derive(Debug)]
pub struct Arena {
    // Safety invariant: this must always be a valid arena
    raw: RawArena,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

// SAFETY: `Arena` uniquely holds the underlying RawArena and has no
// thread-local data.
unsafe impl Send for Arena {}

impl Arena {
    /// Allocates a fresh arena.
    #[inline]
    pub fn new() -> Self {
        #[inline(never)]
        #[cold]
        fn arena_new_failed() -> ! {
            panic!("Could not create a new UPB arena");
        }

        // SAFETY:
        // - `upb_Arena_New` is assumed to be implemented correctly and always sound to
        //   call; if it returned a non-null pointer, it is a valid arena.
        unsafe {
            let Some(raw) = upb_Arena_New() else { arena_new_failed() };
            Self { raw, _not_sync: PhantomData }
        }
    }

    /// # Safety
    /// - The `raw_arena` must point to a valid arena.
    /// - The caller must ensure that the Arena's destructor does not run.
    pub unsafe fn from_raw(raw_arena: RawArena) -> Self {
        Arena { raw: raw_arena, _not_sync: PhantomData }
    }

    /// Returns the raw, UPB-managed pointer to the arena.
    #[inline]
    pub fn raw(&self) -> RawArena {
        self.raw
    }

    /// Allocates some memory on the arena.
    ///
    /// # Safety
    ///
    /// - `layout`'s alignment must be less than `UPB_MALLOC_ALIGN`.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub unsafe fn alloc(&self, layout: Layout) -> &mut [MaybeUninit<u8>] {
        debug_assert!(layout.align() <= UPB_MALLOC_ALIGN);
        // SAFETY: `self.raw` is a valid UPB arena
        let ptr = unsafe { upb_Arena_Malloc(self.raw, layout.size()) };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }

        // SAFETY:
        // - `upb_Arena_Malloc` promises that if the return pointer is non-null, it is
        //   dereferencable for `size` bytes and has an alignment of `UPB_MALLOC_ALIGN`
        //   until the arena is destroyed.
        // - `[MaybeUninit<u8>]` has no alignment requirement, and `ptr` is aligned to a
        //   `UPB_MALLOC_ALIGN` boundary.
        unsafe { slice::from_raw_parts_mut(ptr.cast(), layout.size()) }
    }

    /// Same as alloc() but panics if `layout.align() > UPB_MALLOC_ALIGN`.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn checked_alloc(&self, layout: Layout) -> &mut [MaybeUninit<u8>] {
        assert!(layout.align() <= UPB_MALLOC_ALIGN);
        // SAFETY: layout.align() <= UPB_MALLOC_ALIGN asserted.
        unsafe { self.alloc(layout) }
    }

    /// Copies the T into this arena and returns a pointer to the T data inside
    /// the arena.
    pub fn copy_in<'a, T: Copy>(&'a self, data: &T) -> &'a T {
        let layout = Layout::for_value(data);
        let alloc = self.checked_alloc(layout);

        // SAFETY:
        // - alloc is valid for `layout.len()` bytes and is the uninit bytes are written
        //   to not read from until written.
        // - T is copy so copying the bytes of the value is sound.
        unsafe {
            let alloc = alloc.as_mut_ptr().cast::<MaybeUninit<T>>();
            // let data = (data as *const T).cast::<MaybeUninit<T>>();
            (*alloc).write(*data)
        }
    }

    pub fn copy_str_in<'a>(&'a self, s: &str) -> &'a str {
        let copied_bytes = self.copy_slice_in(s.as_bytes());
        // SAFETY: `copied_bytes` has same contents as `s` and so must meet &str
        // criteria.
        unsafe { std::str::from_utf8_unchecked(copied_bytes) }
    }

    pub fn copy_slice_in<'a, T: Copy>(&'a self, data: &[T]) -> &'a [T] {
        let layout = Layout::for_value(data);
        let alloc: *mut T = self.checked_alloc(layout).as_mut_ptr().cast();

        // SAFETY:
        // - uninit_alloc is valid for `layout.len()` bytes and is the uninit bytes are
        //   written to not read from until written.
        // - T is copy so copying the bytes of the values is sound.
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), alloc, data.len());
            slice::from_raw_parts_mut(alloc, data.len())
        }
    }

    /// Fuse two arenas so they share the same lifetime.
    ///
    /// `fuse` will make it so that the memory allocated by `self` or `other` is
    /// guaranteed to last until both `self` and `other` have been dropped.
    /// The pointers returned by `Arena::alloc` will continue to be valid so
    /// long as either `self` or `other` has not been dropped.
    pub fn fuse(&self, other: &Arena) {
        // SAFETY: `self.raw()` and `other.raw()` are both valid UPB arenas.
        let success = unsafe { upb_Arena_Fuse(self.raw(), other.raw()) };
        if !success {
            // Fusing can fail if any of the arenas has an initial block i.e. the arena is
            // backed by a preallocated chunk of memory that it doesn't own and thus cannot
            // lifetime extend. This function panics because this is typically not a
            // recoverable error but a logic bug in a program.
            panic!("Could not fuse two UPB arenas.");
        }
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Arena {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            upb_Arena_Free(self.raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arena_new_and_free() {
        let arena = Arena::new();
        drop(arena);
    }
}
