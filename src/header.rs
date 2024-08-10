#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

use crate::basics::MAX_BYTES_OUT;

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct MessageHeader {
    pub(crate) object_id: u32,
    pub(crate) opcode: u16,
    pub(crate) size: u16,
}

impl MessageHeader {
    pub(crate) fn new(header: &[u32]) -> Self {
        let object_id = header[0];
        let word_2 = header[1];
        let opcode = (word_2 & 0xffff) as u16;
        let size = (word_2 >> 16) as u16;
        assert!(size as usize <= MAX_BYTES_OUT);
        Self { object_id, opcode, size }
    }

    pub(crate) fn as_words(self) -> [u32; 2] {
        let word1 = self.object_id;
        #[allow(clippy::cast_possible_truncation)]
        #[allow(clippy::cast_lossless)]
        let word2 = ((self.size as u32) << 16) | self.opcode as u32;
        [word1, word2]
    }

    pub(crate) fn len32(&self) -> usize {
        (self.size as usize + 3) / 4
    }
}

#[inline]
pub(crate) fn get_msg_length(header: &[u32]) -> Option<usize> {
    let word_2 = header.get(1)?;
    Some(((word_2 >> 16) as usize + 3) / 4)
}
