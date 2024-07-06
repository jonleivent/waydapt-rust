use crate::basics::{bytes2u32, MAX_BYTES_OUT};

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct MessageHeader {
    pub(crate) object_id: u32,
    pub(crate) opcode: u16,
    pub(crate) size: u16,
}

impl MessageHeader {
    pub(crate) fn new(header: &[u8]) -> Self {
        let object_id = bytes2u32(&header[0..4]);
        let word_2 = bytes2u32(&header[4..8]);
        let opcode = (word_2 & 0xffff) as u16;
        let size = (word_2 >> 16) as u16;
        assert!(size as usize <= MAX_BYTES_OUT);
        Self { object_id, opcode, size }
    }

    pub(crate) fn as_bytes(&self) -> [u8; 8] {
        let word1 = self.object_id.to_ne_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let word2 = (((self.size as u32) << 16) | self.opcode as u32).to_ne_bytes();
        [word1, word2].concat().try_into().unwrap()
    }
}

#[inline(always)]
pub(crate) fn get_msg_length(header: &[u8]) -> Option<usize> {
    let word_2 = u32::from_ne_bytes(header.get(4..8)?.try_into().ok()?);
    Some((word_2 >> 16) as usize)
}
