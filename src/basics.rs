#![allow(dead_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::inline_always)]

use crate::crate_traits::AllBitValuesSafe;

pub(crate) const MAX_FDS_OUT: usize = 28;

pub(crate) const MAX_BYTES_OUT: usize = 4096;
pub(crate) const MAX_WORDS_OUT: usize = MAX_BYTES_OUT / 4;

pub(crate) const MAX_ARGS: usize = 20; // WL_CLOSURE_MAX_ARGS in wayland

#[inline(always)]
pub(crate) const fn round4(x: usize) -> usize {
    (x + 3) & !3
}

#[inline(always)]
pub(crate) const fn init_array<T: Copy, const N: usize>(init_element: T) -> [T; N] {
    [init_element; N]
}

#[inline(always)]
pub(crate) const fn uninit_array<T: AllBitValuesSafe, const N: usize>() -> [T; N] {
    use std::mem::MaybeUninit;
    // The std manual warns that this is UB:
    //
    // "Moreover, uninitialized memory is special in that it does not have a fixed value (“fixed”
    // meaning “it won’t change without being written to”). Reading the same uninitialized byte
    // multiple times can give different results. This makes it undefined behavior to have
    // uninitialized data in a variable even if that variable has an integer type, which otherwise
    // can hold any fixed bit pattern:"
    //
    // But then, how could ArrayVec::set_len be allowed?  Or how does allocation of a new ArrayVec
    // work?
    //
    // It must be the case that, once we write in any way to an uninit element, it becomes stable
    // with respect to the behavior hinted at above.  Which is all we care about.
    let x: MaybeUninit<[T; N]> = MaybeUninit::uninit();
    unsafe { x.assume_init() }
}

#[inline(always)]
pub(crate) fn to_u8_slice_mut<T: AllBitValuesSafe>(s: &mut [T]) -> &mut [u8] {
    let (start, s2, end) = unsafe { s.align_to_mut::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

#[inline(always)]
pub(crate) fn to_u8_slice<T: AllBitValuesSafe>(s: &[T]) -> &[u8] {
    let (start, s2, end) = unsafe { s.align_to::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

// Maybe use the num_traits crate instead? https://docs.rs/num-traits/latest/num_traits/
unsafe impl AllBitValuesSafe for u8 {}
unsafe impl AllBitValuesSafe for u16 {}
unsafe impl AllBitValuesSafe for u32 {}
unsafe impl AllBitValuesSafe for u64 {}
unsafe impl AllBitValuesSafe for u128 {}
unsafe impl AllBitValuesSafe for usize {}

unsafe impl AllBitValuesSafe for i8 {}
unsafe impl AllBitValuesSafe for i16 {}
unsafe impl AllBitValuesSafe for i32 {}
unsafe impl AllBitValuesSafe for i64 {}
unsafe impl AllBitValuesSafe for i128 {}
unsafe impl AllBitValuesSafe for isize {}
