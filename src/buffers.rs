#![warn(clippy::pedantic)]
//#![forbid(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

use crate::basics::{to_u8_slice_mut, uninit_array, MAX_ARGS, MAX_FDS_OUT, MAX_WORDS_OUT};
use crate::crate_traits::Messenger;
use crate::header::{get_msg_length, MessageHeader};
use crate::streams::{recv_msg, send_msg, IOStream};
use arrayvec::ArrayVec;
use rustix::fd::BorrowedFd;
use std::collections::{LinkedList, VecDeque};
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, OwnedFd};

#[derive(Debug)]
pub(crate) struct InBuffer<'a> {
    data: [u32; MAX_WORDS_OUT * 2],
    front: usize,
    back: usize,
    fds: VecDeque<OwnedFd>,
    stream: &'a IOStream,
}

impl<'a> InBuffer<'a> {
    pub(crate) fn new(stream: &'a IOStream) -> Self {
        Self {
            data: uninit_array(),
            front: 0,
            back: 0,
            fds: VecDeque::with_capacity(MAX_FDS_OUT),
            stream,
        }
    }

    // pop a single message from the buffer, based on the length in its header, if it is all there
    pub(crate) fn try_pop(&mut self) -> Option<(&[u32], &mut VecDeque<OwnedFd>)> {
        debug_assert!(self.back <= self.data.len() && self.front <= self.back);
        let available = &self.data[self.front..self.back];
        // Determine the msg len from its header, if enough is there for a header:
        let msg_len = get_msg_length(available)?;
        // Check if the whole message is available:
        let msg = available.get(0..msg_len)?;
        // must be &mut self because of this:
        self.front += msg_len;
        debug_assert!(self.front <= self.back);
        // For borrow-checker reasons, we have to return the msg and a &mut ref to the fds VecDeque
        // so that the caller retains the ability to get fds from that during the lifetime of the
        // msg, which borrows from &mut self:
        Some((msg, &mut self.fds))
    }

    pub(crate) fn receive(&mut self) -> IoResult<usize> {
        self.compact();
        debug_assert!(self.back <= MAX_WORDS_OUT);
        // if we have more than MAX_WORDS_OUT available storage, should we allow receive to use it?
        // That might cause problems with fds - if we receive too much, we might leave fds in the
        // kernel socket buffer that we need.  How would this happen?  Suppose that the kernel
        // socket buffer contains MAX_WORDS_OUT + D data and MAX_FDS_OUT + F fds, and we have room
        // for all MAX_WORDS_OUT + D data.  We will only take in MAX_FDS_OUT fds, however, and if
        // the msgs in the first MAX_WORDS_OUT use all MAX_FDS_OUT fds, then the msgs in the last D
        // will be starved of fds.  So we have to limit the message input to MAX_WORDS_OUT.
        let buf = &mut self.data[self.back..(MAX_WORDS_OUT + self.back)];
        debug_assert_eq!(buf.len(), MAX_WORDS_OUT);
        let amount_read = recv_msg(self.stream, buf, &mut self.fds)?;
        self.back += amount_read;
        debug_assert!(self.back <= self.data.len());
        Ok(amount_read)
    }

    fn compact(&mut self) {
        // Doing a compaction ensures that self.data[self.back..] is big enough to hold a max-sized
        // message, which is MAX_WORDS_OUT.  We don't use a ring buffer (like C libwayland) because
        // the savings of not doing any compaction copy is more than offset by the added complexity
        // of dealing with string or array message args that wrap.
        let len = self.back - self.front;
        debug_assert!(len <= MAX_WORDS_OUT);
        // Compacting more often than absolutely necessary has a cost, but it may improve CPU
        // cache friendliness.
        if len == 0 {
            // nothing to move, just reset front and back
        } else if self.front >= len // no overlap
            && (self.back > MAX_WORDS_OUT || COMPACT_SCHEME == CompactScheme::Eager)
        {
            // we have to, or at least want to compact now, and can use memcpy because no overlap
            // between source and destination:
            let (left, right) = self.data.split_at_mut(self.front);
            left[..len].copy_from_slice(&right[..len]);
        } else if self.back > MAX_WORDS_OUT {
            // we have to compact now, but there is an overlap, so use memmove:
            self.data.copy_within(self.front..self.back, 0);
        } else {
            debug_assert!(self.data[self.back..].len() >= MAX_WORDS_OUT);
            return;
        }
        self.back = len;
        self.front = 0;
    }
}

const CHUNK_SIZE: usize = MAX_WORDS_OUT * 2;

type Chunk = ArrayVec<u32, CHUNK_SIZE>;
// Either of the following works:
type Chunks = LinkedList<Chunk>;
//type Chunks = VecDeque<Box<Chunk>>;

#[derive(Debug)]
pub(crate) struct OutBuffer<'a> {
    chunks: Chunks,
    free_chunks: Chunks,
    fds: VecDeque<OwnedFd>,
    pub(crate) flush_every_send: bool,
    wait_for_output_event: bool,
    stream: &'a IOStream,
}

fn initial_chunks() -> Chunks {
    #![allow(clippy::default_trait_access)]
    // Chunks with a single empty Chunk
    let mut new: Chunks = Default::default();
    new.push_back(Default::default());
    new
}

const LIMIT_SENDS_TO_MAX_WORDS_OUT: bool = false;

impl<'a> OutBuffer<'a> {
    pub(crate) fn new(stream: &'a IOStream) -> Self {
        #![allow(clippy::default_trait_access)]
        Self {
            // chunks must never be empty:
            chunks: initial_chunks(),
            // free_chunks can start life empty:
            free_chunks: Default::default(),
            // fds will never grow larger than MAX_FDS_OUT + MAX_ARGS because MAX_ARGS is the most
            // any one message can push on it, and whenever it gets larger than MAX_FDS_OUT, it will
            // force a flush:
            fds: VecDeque::with_capacity(MAX_FDS_OUT + MAX_ARGS),
            flush_every_send: false,
            wait_for_output_event: false,
            stream,
        }
    }

    fn start_next_chunk(&mut self) {
        let mut new_end = if self.free_chunks.is_empty() {
            initial_chunks()
        } else {
            self.free_chunks.split_off(self.free_chunks.len() - 1)
        };
        debug_assert_eq!(new_end.len(), 1);
        let new_end_chunk = new_end.front_mut().expect(MUST_HAVE1);
        assert!(new_end_chunk.is_empty());

        // In some cases (MessageSender::send), we don't know until after we've put a message in the
        // end chunk whether it extends past MAX_WORDS_OUT.  Check now, but only if we care about
        // LIMIT_SENDS_TO_MAX_WORDS_OUT:
        let old_end_chunk = self.end_mut();
        if LIMIT_SENDS_TO_MAX_WORDS_OUT && old_end_chunk.len() > MAX_WORDS_OUT {
            // old_end_chunk is too long for our taste, so move the end of it to the new_end_chunk
            new_end_chunk.extend(old_end_chunk.drain(MAX_WORDS_OUT..));
        }
        self.chunks.append(&mut new_end);
    }

    #[inline]
    fn is_empty(&self) -> bool {
        #![allow(dead_code)]
        // The whole OutBuffer is considered empty if its first chunk is empty
        self.chunks.front().expect(MUST_HAVE1).is_empty()
    }

    #[inline]
    fn end(&self) -> &Chunk { self.chunks.back().expect(MUST_HAVE1) }

    #[inline]
    fn end_mut(&mut self) -> &mut Chunk { self.chunks.back_mut().expect(MUST_HAVE1) }

    #[inline]
    fn too_many_fds(&self) -> bool {
        // This condition is dangerous to the receiver, because we are limited to sending
        // MAX_FDS_OUT fds with each chunk.  If we sent all of the chunks and still had fds to send,
        // the receiver might become starved of fds when trying to process the last message(s) sent,
        // but we can't send fds without data to help.
        self.fds.len() > self.chunks.len() * MAX_FDS_OUT
    }

    pub(crate) fn got_output_event(&mut self) { self.wait_for_output_event = false; }

    pub(crate) fn flush(&mut self, force: bool) -> IoResult<usize> {
        let mut total_flushed = 0;
        // self.chunks.len() > 1 means that there are chunks waiting to get flushed that were
        // refused prevoiusly because the kernel socket buffer did not have room - those should
        // always get flushed if there is now room, as that maximizes message throughput by not
        // requiring that the receiver wait longer than it has to while not trading off for fewer
        // calls to send.  The final chunk should only get flushed after all pending message traffic
        // has been processed - meaning just before starting what might be a traffic lull - as that
        // does make for fewer calls to send, even though it might result in the receiver waiting
        // longer than it has to.  Or, when we get signalled that there's now room in the kernel
        // socket buffer for messages we tried to flush previously.  Or because we were told to
        // flush in preference to fewer send calls.  Both of those final cases have force=true.
        while self.chunks.len() > 1 || force {
            let amount_flushed = self.flush_first_chunk()?;
            if amount_flushed == 0 {
                break;
            }
            total_flushed += amount_flushed;
        }
        Ok(total_flushed)
    }

    fn flush_first_chunk(&mut self) -> IoResult<usize> {
        let first_chunk = self.chunks.front_mut().expect(MUST_HAVE1);
        if self.wait_for_output_event || first_chunk.is_empty() {
            // Nothing to do
            return Ok(0);
        }
        let (nwords_flushed, nfds_flushed) = {
            let bfds: ArrayVec<BorrowedFd, MAX_FDS_OUT> =
                self.fds.iter().take(MAX_FDS_OUT).map(OwnedFd::as_fd).collect();
            (send_msg(self.stream, first_chunk, &bfds)?, bfds.len())
        };
        debug_assert!(nwords_flushed > 0 || nfds_flushed == 0);
        // if amount_flushed == 0, then not even the fds were flushed
        if nwords_flushed > 0 {
            // If the flush happened, then we expect the whole chunk was taken:
            debug_assert_eq!(nwords_flushed, first_chunk.len());
            // remove flushed fds, which are owned, so this will do closes:
            self.fds.drain(..nfds_flushed);
            // remove flushed msg data:
            first_chunk.clear();
            if self.chunks.len() > 1 {
                // there are other active chunks, so first_chunk needs to be popped and possibly
                // moved to inactive_chunks
                let rest_active_chunks = self.chunks.split_off(1);
                // self.chunks should now only contain the first_chunk
                debug_assert_eq!(self.chunks.len(), 1);
                // if we are below the excess threshold, keep it in free_chunks, else let it drop
                if self.free_chunks.len() < ALLOWED_EXCESS_CHUNKS {
                    self.free_chunks.append(&mut self.chunks);
                }
                self.chunks = rest_active_chunks;
                debug_assert!(!self.chunks.is_empty());
            }
        } else {
            // If the flush did nothing, there is no need to try again until we've been told by the
            // event loop that there's room:
            self.wait_for_output_event = true;
        }
        Ok(nwords_flushed)
    }

    fn add_fds(&mut self, fds: impl IntoIterator<Item = OwnedFd>) -> IoResult<()> {
        self.fds.extend(fds);
        // Prevent the possibility of having fds in the OutBuffer but no msg data, which can
        // cause starvation of the receiver.  This can happen whenever there are more fds than
        // can be flushed with the existing data, due to the MAX_FDS_OUT limit.
        if self.too_many_fds() {
            self.flush(true)?;
            if self.too_many_fds() {
                self.start_next_chunk();
                // We should never need more than one start_next_chunk call to bring the number of
                // fds per chunk to a safe level
                debug_assert!(!self.too_many_fds());
            }
        }
        Ok(())
    }
}

impl<'a> Messenger for OutBuffer<'a> {
    // Allow a closure (msgfun) to marshal the message into this OutBuffer using an ExtendChunk for
    // each arg of th message, and returning the MessageSender for us to fix up (fix the size field)
    // and marshal.
    fn send(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>,
        msgfun: impl FnOnce(ExtendChunk) -> MessageHeader,
    ) -> IoResult<usize> {
        // for each msg, push fds first, then push the msg.  This is because we want any fds to be
        // associated with the earliest possible data chunk to help prevent fd starvation of the
        // receiver.
        self.add_fds(fds)?;

        // Is there enough space in the end chunk for a max-sized message?  If not, start a new
        // chunk.  In this version of send (unlike send_raw), we don't try to completely fill chunks
        // before moving on to a new one, because we don't know the message length before we allow
        // msgfun to write the message.  We could have made ExtendChunk more elaborate and test each
        // case for splitting, but that would have added complexity and possibly also made
        // ExtendChunk slower.  Instead, we rely on the fact that each chunk has 2 * MAX_WORDS_OUT
        // capacity, while each message can have max MAX_WORDS_OUT length.  Also, if
        // LIMIT_SENDS_TO_MAX_WORDS_OUT is set, start_next_chunk will do splitting for us (at the
        // cost of an additional copy of the split off end).
        if self.end().remaining_capacity() < MAX_WORDS_OUT {
            self.start_next_chunk();
        };
        debug_assert!(self.end().remaining_capacity() >= MAX_WORDS_OUT);

        let end_chunk = self.end_mut();
        let orig_len = end_chunk.len();
        // make room for header, which we will write below:
        end_chunk.extend(vec![0u32; 2]);
        let mut header = msgfun(ExtendChunk(end_chunk));
        let new_len = end_chunk.len();
        let len = new_len - orig_len;
        assert!((2..=MAX_WORDS_OUT).contains(&len));
        // fix the header.size field to the now known length and write the header:
        {
            #![allow(clippy::cast_possible_truncation)]
            // we know this won't truncate because MAX_WORDS_OUT is <= the max u16:
            debug_assert!(u16::try_from(MAX_WORDS_OUT).is_ok());
            header.size = 4 * (len as u16);
        };
        // Now, write the updated header:
        end_chunk[orig_len..orig_len + 2].copy_from_slice(&header.as_words());

        // This flush is not forced (unless flush_every_send is true) because somewhere up the
        // call chain there is someone responsible for forcing a flush after all pending message
        // traffic has been processed.  This flush is only for excess (after the first chunk),
        // and is not necessary.  It's benefit is that it may raise the throughput by allowing
        // more parallelism between sender and receiver.  It may also limit the number of chunks
        // needed in this OutBuffer.
        self.flush(self.flush_every_send)
    }

    // Allow a closure (msgfun) to copy a message in its raw form (including header) from the
    // InBuffer to the OutBuffer.
    fn send_raw(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>, raw_msg: &[u32],
    ) -> IoResult<usize> {
        debug_assert_eq!(get_msg_length(raw_msg).unwrap(), raw_msg.len());

        self.add_fds(fds)?;

        let msg_len = raw_msg.len();
        let end_chunk = self.end_mut();
        let room = if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            MAX_WORDS_OUT - end_chunk.len()
        } else {
            end_chunk.remaining_capacity()
        };
        if room >= msg_len {
            // msg fits in end chunk
            end_chunk.try_extend_from_slice(raw_msg).unwrap();
        } else {
            // split across 2 chunks
            let (first_part, last_part) = raw_msg.split_at(room);
            end_chunk.try_extend_from_slice(first_part).unwrap();
            self.start_next_chunk();
            self.end_mut().try_extend_from_slice(last_part).unwrap();
        }

        self.flush(self.flush_every_send)
    }
}

#[derive(Debug, PartialEq)]
pub enum CompactScheme {
    Eager,
    Lazy,
}

const COMPACT_SCHEME: CompactScheme = CompactScheme::Lazy;

const MUST_HAVE1: &str = "active_chunks must always have at least 1 element";
const ALLOWED_EXCESS_CHUNKS: usize = 8;

// ExtendChunk is a restricted-interface around Chunk that only allows adding arguments from a
// message.  Why use a wrapper struct instead of a trait?  Because this will be the arg type of a
// closure (the msgfun in send above), which can't take an impl Trait, and we don't need to use a
// dyn Trait just for this.  And we will only ever need one implementation in terms of Chunk.
pub(crate) struct ExtendChunk<'a>(pub(self) &'a mut Chunk);

impl<'a> ExtendChunk<'a> {
    #![allow(clippy::inline_always)]
    #[inline(always)]
    pub(crate) fn add_u32(&mut self, data: u32) { self.0.push(data); }

    #[inline(always)]
    pub(crate) fn add_i32(&mut self, data: i32) {
        #[allow(clippy::cast_sign_loss)]
        self.0.push(data as u32);
    }

    // Add an uninitialized slice of nwords, and then allow the caller to fill it.
    #[inline(always)]
    unsafe fn add_mut_slice(&mut self, nwords: usize) -> &mut [u32] {
        let orig_len = self.0.len();
        let new_len = orig_len + nwords;
        unsafe {
            self.0.set_len(new_len);
        }
        &mut self.0[orig_len..new_len]
    }

    #[inline(always)]
    pub(crate) fn add_array(&mut self, data: &[u8]) {
        #![allow(clippy::cast_possible_truncation)]
        let nbytes = data.len();
        self.add_u32(nbytes as u32);
        let nwords = (nbytes + 3) / 4;
        let words = unsafe { self.add_mut_slice(nwords) };
        let buf = to_u8_slice_mut(words);
        buf[..nbytes].copy_from_slice(data);
    }
}
