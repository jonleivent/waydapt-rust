pub(crate) const MAX_FDS_OUT: usize = 28;

pub(crate) const MAX_BYTES_OUT: usize = 4096;

pub(crate) const MAX_ARGS: usize = 20; // WL_CLOSURE_MAX_ARGS in wayland

#[inline(always)]
pub(crate) const fn round4(x: usize) -> usize {
    (x + 3) & !3
}

#[inline(always)]
pub(crate) fn bytes2u32(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[0..4].try_into().unwrap())
}

#[inline(always)]
pub(crate) fn bytes2i32(bytes: &[u8]) -> i32 {
    i32::from_ne_bytes(bytes[0..4].try_into().unwrap())
}

#[inline(always)]
pub(crate) fn init_array<T: Copy, const N: usize>(init_element: T) -> [T; N] {
    [init_element; N]
}

// What has to be done to use Peer intead of index for buffers/streams, and also in maps?
//
// Rust only allows bool and str as generic constants.  We could make Peer a trait with an associated constant INDEX (0 for client, 1 for server), with two types implementing it.
