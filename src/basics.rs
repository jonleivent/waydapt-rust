#![allow(dead_code)]
#![allow(clippy::inline_always)]
#![allow(unsafe_code)]

use std::thread::panicking;

use crate::crate_traits::Alloc;

pub(crate) const MAX_FDS_OUT: usize = 28;

pub const MAX_BYTES_OUT: usize = 4096;
pub(crate) const MAX_WORDS_OUT: usize = MAX_BYTES_OUT / 4;

const _: () = assert!(MAX_BYTES_OUT < u16::MAX as usize);

pub(crate) const MAX_ARGS: usize = 20; // WL_CLOSURE_MAX_ARGS in wayland

#[inline(always)]
pub(crate) const fn round4(x: usize) -> usize { (x + 3) & !3 }

pub(crate) struct Leaker;

pub(crate) const LEAKER: Leaker = Leaker;

impl Alloc for Leaker {
    #[inline]
    fn alloc<T>(&self, it: T) -> &mut T { Box::leak(Box::new(it)) }
}

#[inline(always)]
pub(crate) fn to_u8_slice_mut(s: &mut [u32]) -> &mut [u8] {
    // Safety: there's no way for a 4-byte aligned &mut [u32] to have any left-over start or end
    // parts when converting to a 1-byte aligned &mut [u8], and it is safe to view u32's as
    // native-endian sequences of u8's.
    let (start, s2, end) = unsafe { s.align_to_mut::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

#[inline(always)]
pub(crate) fn to_u8_slice(s: &[u32]) -> &[u8] {
    // Safety: there's no way for a 4-byte aligned &[u32] to have any left-over start or end parts
    // when converting to a 1-byte aligned &[u8], and it is safe to view u32's as native-endian
    // sequences of u8's.
    let (start, s2, end) = unsafe { s.align_to::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

// maybe use crate derive_more instead of this wrapper?:
#[derive(Copy, Clone, PartialEq, Default)]
#[repr(transparent)]
pub struct NoDebug<T: ?Sized>(pub T);

impl<T: ?Sized> std::fmt::Debug for NoDebug<T> {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "<...>")
    }
}

impl<T: ?Sized> std::ops::Deref for NoDebug<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl<T: ?Sized> std::ops::DerefMut for NoDebug<T> {
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

pub struct UnwindDo<F: FnOnce() + Copy>(pub F);

impl<F: FnOnce() + Copy> Drop for UnwindDo<F> {
    fn drop(&mut self) {
        if panicking() {
            self.0();
        }
    }
}
