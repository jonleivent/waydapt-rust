use crate::crate_traits::IsNumericType;

pub(crate) const MAX_FDS_OUT: usize = 28;

pub(crate) const MAX_BYTES_OUT: usize = 4096;
pub(crate) const MAX_WORDS_OUT: usize = MAX_BYTES_OUT / 4;

pub(crate) const MAX_ARGS: usize = 20; // WL_CLOSURE_MAX_ARGS in wayland

#[inline(always)]
pub(crate) const fn round4(x: usize) -> usize {
    (x + 3) & !3
}

#[inline(always)]
pub(crate) fn init_array<T: Copy, const N: usize>(init_element: T) -> [T; N] {
    [init_element; N]
}

#[inline(always)]
pub(crate) unsafe fn to_u8_slice_mut<T: IsNumericType>(s: &mut [T]) -> &mut [u8] {
    let (start, s2, end) = unsafe { s.align_to_mut::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

#[inline(always)]
pub(crate) unsafe fn to_u8_slice<T: IsNumericType>(s: &[T]) -> &[u8] {
    let (start, s2, end) = unsafe { s.align_to::<u8>() };
    debug_assert!(start.is_empty() && end.is_empty());
    s2
}

impl IsNumericType for u8 {}
impl IsNumericType for u16 {}
impl IsNumericType for u32 {}
impl IsNumericType for u64 {}
impl IsNumericType for u128 {}
impl IsNumericType for usize {}

impl IsNumericType for i8 {}
impl IsNumericType for i16 {}
impl IsNumericType for i32 {}
impl IsNumericType for i64 {}
impl IsNumericType for i128 {}
impl IsNumericType for isize {}

impl IsNumericType for bool {}
