pub(crate) enum Peer {
    Client = 0,
    Server = 1,
}

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

#[inline(always)]
pub(crate) fn get_msg_length(header: &[u8]) -> Option<usize> {
    let word_2 = u32::from_ne_bytes(header.get(4..8)?.try_into().ok()?);
    Some((word_2 >> 16) as usize)
}

#[inline(always)]
pub(crate) fn load_slice(dest: &mut [u8], src: &[u8]) -> usize {
    dest[..src.len()].copy_from_slice(src);
    src.len()
}
