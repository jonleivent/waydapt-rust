use crate::basics::{MAX_ARGS, MAX_FDS_OUT, MAX_WORDS_OUT};
use crate::crate_traits::Messenger;
use crate::header::{get_msg_nwords, MessageHeader};
use crate::streams::{recv_msg, send_msg, IOStream};
use arrayvec::ArrayVec;
use rustix::fd::BorrowedFd;
#[cfg(not(feature = "no_linked_list"))]
use std::collections::LinkedList;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, OwnedFd};

const LIMIT_SENDS_TO_MAX_WORDS_OUT: bool = cfg!(feature = "limit_sends");
const FAST_COMPACT: bool = cfg!(feature = "fast_compact");
const ALWAYS_COMPACT: bool = cfg!(feature = "always_compact");

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
            data: [0u32; MAX_WORDS_OUT * 2],
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
        let msg_len = get_msg_nwords(available)?;
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
        self.make_room();
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

    // Make sure there's room to receive new message(s).  We need MAX_WORDS_OUT space, since that's
    // the most content we'd ever get in a single receive.
    fn make_room(&mut self) {
        let len = self.back - self.front; // existing content
        debug_assert!(len <= MAX_WORDS_OUT);
        if len > 0 {
            // there's some content
            if ALWAYS_COMPACT || self.back > MAX_WORDS_OUT {
                // and a reason to move it
                self.compact();
            }
        } else {
            // we're empty - just reset front and back
            self.back = 0;
            self.front = 0;
        }
        debug_assert!(self.data[self.back..].len() >= MAX_WORDS_OUT);
    }

    // Compaction is rare.  It requies that we did not consume the previous input completely before
    // receiving more.  Because we consume all complete messages before requesting more input from
    // the kernel socket, the only way we would not have consumed the previous input is if the last
    // message was only partially received.  And that is rare because Wayland clients and servers
    // use all-or-nothing sends of chunks, so only messages that span chunks can be partially
    // received.  Since a Wayland chunk is 4K, and messages are on the order of 16 bytes, only about
    // half a percent of messages could be subject to being partially received.  Even then, senders
    // will rarely batch enough messages together to reach that point.
    #[cold]
    fn compact(&mut self) {
        let len = self.back - self.front;
        // Doing a compaction ensures that self.data[self.back..] is big enough to hold a max-sized
        // message, which is MAX_WORDS_OUT.  We don't use a ring buffer (like C libwayland) because
        // the savings of not doing any compaction (which is already exceedingly are) is more than
        // offset by the added complexity of dealing with string or array message args that wrap in
        // a ring buffer.
        if self.front >= len && FAST_COMPACT {
            // we can use memcpy because no overlap between source and destination:
            let (left, right) = self.data.split_at_mut(self.front);
            left[..len].copy_from_slice(&right[..len]);
        } else {
            // there is an overlap, so use memmove:
            //
            // All compaction is rare, and this case is the rarest form of compaction.  It has
            // self.front < len, which means the partial message [self.front..self.back] is either
            // partial because it is very long, or because the previous receive was cut off well
            // before delivering MAX_WORDS_OUT.  Long messages are uncommon.  The second reason is
            // even less common - it would imply that the sender is not a typical Wayland client or
            // server (libwayland-based), because those don't send partial messages unless their
            // MAX_WORDS_OUT sized buffer fills, which would put self.front >= len.  And,
            // libwayland-based (and us as well) clients and servers do all-or-nothing sends, so the
            // issue is not the kernel socket buffer's size.
            self.data.copy_within(self.front..self.back, 0);
        }
        self.back = len;
        self.front = 0;
    }
}

const CHUNK_SIZE: usize = MAX_WORDS_OUT * 2;

type Chunk = ArrayVec<u32, CHUNK_SIZE>;

// Either of the following works:
#[cfg(not(feature = "no_linked_list"))]
type Chunks = LinkedList<Chunk>;
#[cfg(feature = "no_linked_list")]
type Chunks = VecDeque<Box<Chunk>>;

const ALLOWED_EXCESS_CHUNKS: usize = 4;

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

const MUST_HAVE1: &str = "active_chunks must always have at least 1 element";

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

    #[cold]
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

    // #[inline]
    // fn is_empty(&self) -> bool {
    //     #![allow(dead_code)]
    //     // The whole OutBuffer is considered empty if its first chunk is empty
    //     self.chunks.front().expect(MUST_HAVE1).is_empty()
    // }

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
            // This is an exceptionally rare circumstance, requiring many messages containing
            // fds to be sent nearly consecutively.
            self.flush(true)?;
            if self.too_many_fds() {
                // Rarer still - the above condition but with an ineffective flush due to kernel
                // socket buffer backup (probably the receiving peer is slow or stopped).
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
        // LIMIT_SENDS_TO_MAX_WORDS_OUT is set, the second start_next_chunk below will do splitting
        // for us (at the cost of an additional copy of the split off end).
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

        if LIMIT_SENDS_TO_MAX_WORDS_OUT && self.end().remaining_capacity() < MAX_WORDS_OUT {
            // will split off the excess above MAX_WORDS_OUT and move it to a new chunk
            self.start_next_chunk();
        };

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
        debug_assert_eq!(get_msg_nwords(raw_msg).unwrap(), raw_msg.len());

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

// ExtendChunk is a restricted-interface around Chunk that only allows adding arguments from a
// message.  Why use a wrapper struct instead of a trait?  Because this will be the arg type of a
// closure (the msgfun in send above), which can't take an impl Trait, and we don't need to use a
// dyn Trait just for this.  And we will only ever need one implementation in terms of Chunk.
pub(crate) struct ExtendChunk<'a>(pub(self) &'a mut Chunk);

impl<'a> ExtendChunk<'a> {
    #![allow(clippy::inline_always)]
    #![allow(unsafe_code)]

    #[inline(always)]
    pub(crate) fn add_u32(&mut self, data: u32) { self.0.push(data); }

    #[inline(always)]
    pub(crate) fn add_i32(&mut self, data: i32) {
        #[allow(clippy::cast_sign_loss)]
        self.0.push(data as u32);
    }

    pub(crate) fn add_array(&mut self, data: &[u8]) {
        // Note that there is no guarantee of data having > 1 alignment, because the array/string
        // might have been modified (and have its length changed) by an addon
        #![allow(clippy::cast_possible_truncation)]
        let nbytes = data.len();
        self.add_u32(nbytes as u32); // length field goes first
        let rem = nbytes % 4;
        let cut = nbytes - rem;
        for i in (0..cut).step_by(4) {
            self.add_u32(u32::from_ne_bytes(data[i..i + 4].try_into().unwrap()));
        }
        let last = match rem {
            1 => u32::from_ne_bytes([data[cut], 0, 0, 0]),
            2 => u32::from_ne_bytes([data[cut], data[cut + 1], 0, 0]),
            3 => u32::from_ne_bytes([data[cut], data[cut + 1], data[cut + 2], 0]),
            _ => return,
        };
        self.add_u32(last);
    }
}

#[cfg(test)]
pub(crate) mod privates {
    pub(crate) type Chunk = super::Chunk;
    pub(crate) use super::ExtendChunk;

    pub(crate) fn mk_extend_chunk(chunk: &mut Chunk) -> ExtendChunk<'_> { ExtendChunk(chunk) }
}

#[cfg(test)]
mod tests {
    use std::array;
    use std::os::unix::net::UnixStream;

    use crate::basics::{test_util::*, to_u8_slice};

    use super::*;

    // A quick-and-dirty fake message with deterministic but distinct contents:
    fn fake_msg(object_id: u32, nwords: u16) -> Vec<u32> {
        assert!(nwords >= 2);
        let mut msg = Vec::with_capacity(nwords as usize);
        let hdr = MessageHeader { object_id, opcode: 42, size: 4 * nwords };
        msg.extend_from_slice(&hdr.as_words());
        for i in 2..nwords {
            msg.push(u32::from(i) ^ object_id);
        }
        msg
    }

    // a quick-and-dirty fake message with deterministic but distinct contents that is fit for
    // sending via send_fake_msg_for_send, which exercises send instead of send_raw.
    fn fake_msg_for_send(object_id: u32, nwords: u16) -> Vec<u32> {
        assert!(nwords >= 2);
        let mut msg = Vec::with_capacity(nwords as usize);
        let hdr = MessageHeader { object_id, opcode: 42, size: 4 * nwords };
        msg.extend_from_slice(&hdr.as_words());
        if nwords > 2 {
            // make it have 1 arg that is an array - so its array length has to be correct:
            msg.push(u32::from((nwords - 3) * 4));
            for i in 3..nwords {
                msg.push(u32::from(i) ^ object_id);
            }
        }
        msg
    }

    // for sending fake_msg_for_send
    fn send_fake_msg_for_send(
        out: &mut OutBuffer, fds: impl IntoIterator<Item = OwnedFd>, msg: &[u32],
    ) -> IoResult<usize> {
        out.send(fds, |mut ec| {
            ec.add_array(to_u8_slice(&msg[3..]));
            MessageHeader::new(msg)
        })
    }

    #[test]
    fn inbuf_test1() {
        let (s1, s2) = UnixStream::pair().unwrap();
        let mut inbuf = InBuffer::new(&s1);
        // send 2 long msgs, but <= MAX_WORDS_OUT.  InBuffer doesn't care about any part of the msg
        // other than the header length field, which it sees by calling get_msg_nwords on the input.
        // The length field is the high 16 bits of the second (1th) word (u32) of the msg - and it
        // is a nbytes measure - this has to be accurate for try_pop to work.
        let msg1 = fake_msg(1, 512);
        let msg2 = fake_msg(2, 512);

        assert_eq!(send_msg(&s2, &msg1, &[]).unwrap(), msg1.len());
        assert_eq!(send_msg(&s2, &msg2, &[]).unwrap(), msg2.len());
        assert_eq!(inbuf.receive().unwrap(), msg1.len() + msg2.len());
        // the inbuf contains only those two:
        assert_eq!(inbuf.front, 0);
        assert_eq!(inbuf.back, msg1.len() + msg2.len());

        // Pop the first msg and compare contents with the original:
        let (inmsg1, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg1, msg1);
        // This will do a compact (via fast memcpy path if FAST_COMPACT is true)
        inbuf.compact(); // should have no impact on contents, only on position
        assert_eq!(inbuf.front, 0);
        assert_eq!(inbuf.back, msg2.len());

        // Pop the second msg and compare contents with the original:
        let (inmsg2, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg2, msg2);
        // Now empty, but with non-0 front/back:
        assert_eq!(inbuf.front, msg2.len());
        assert_eq!(inbuf.back, msg2.len());
    }

    #[test]
    fn inbuf_test2() {
        let (s1, s2) = UnixStream::pair().unwrap();
        let mut inbuf = InBuffer::new(&s1);
        // send 3 long msgs, with total length > MAX_WORDS_OUT.
        let msg1 = fake_msg(1, 256);
        let msg2 = fake_msg(2, 1024);
        let msg3 = fake_msg(3, 512);

        assert_eq!(send_msg(&s2, &msg1, &[]).unwrap(), msg1.len());
        assert_eq!(send_msg(&s2, &msg2, &[]).unwrap(), msg2.len());
        // we need 2 receives, because its more than 1024 words
        assert_eq!(inbuf.receive().unwrap(), 1024);
        assert_eq!(inbuf.receive().unwrap(), msg1.len() + msg2.len() - 1024);

        assert_eq!(inbuf.front, 0);
        assert_eq!(inbuf.back, msg1.len() + msg2.len());
        assert_eq!(inbuf.data[0..msg1.len()], msg1);
        assert_eq!(inbuf.data[msg1.len()..msg1.len() + msg2.len()], msg2);

        // pop the first msg.
        let (inmsg1, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg1, msg1);

        assert_eq!(inbuf.front, msg1.len()); // empty space from where inmsg1 was popped
        assert_eq!(inbuf.back, msg1.len() + msg2.len());

        // send msg 3 - this will cause make_room to call compact in all cases
        assert_eq!(send_msg(&s2, &msg3, &[]).unwrap(), msg3.len());
        assert_eq!(inbuf.receive().unwrap(), msg3.len());
        assert_eq!(inbuf.front, 0); // compacted space from where inmsg1 was popped
        assert_eq!(inbuf.back, msg2.len() + msg3.len());

        // Now pop the second msg.
        let (inmsg2, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg2, msg2);

        assert_eq!(inbuf.front, msg2.len());
        assert_eq!(inbuf.back, msg2.len() + msg3.len());

        // 3rd msg.
        let (inmsg3, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg3, msg3);

        assert_eq!(inbuf.front, msg2.len() + msg3.len());
        assert_eq!(inbuf.back, msg2.len() + msg3.len());
    }

    #[test]
    fn inout_test1() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        // Just a trivial single msg to sanity test end-to-end buffering
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        let msg = fake_msg(1, 1024);

        assert_eq!(outbuf.send_raw([], &msg).unwrap(), 0); // not flushed
        assert_eq!(outbuf.flush(true).unwrap(), msg.len());
        assert_eq!(inbuf.receive().unwrap(), msg.len());
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msg);
    }

    #[test]
    fn inout_send_test1() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Same as inout_test1, but use send instead of send_raw
        let msg = fake_msg_for_send(1, 1024);

        assert_eq!(send_fake_msg_for_send(&mut outbuf, [], &msg).unwrap(), 0); // not flushed
        assert_eq!(outbuf.flush(true).unwrap(), msg.len());
        assert_eq!(inbuf.receive().unwrap(), msg.len());
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msg);
    }

    #[test]
    fn inout_test2() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Exercise msg splitting by using msgs that don't add up to a multiple of 1024

        // Prevent flushes so we can examine the full impact of the splitting, if any occurred
        outbuf.wait_for_output_event = true;

        // test chunk extending for both the fits in remaining capacty and doesn't fit cases:
        #[allow(clippy::cast_possible_truncation)]
        let msgs: [Vec<u32>; 3] = std::array::from_fn(|i| fake_msg(i as u32, 384));
        for m in &msgs {
            assert_eq!(outbuf.send_raw([], m).unwrap(), 0); // not flushed
        }
        assert_eq!(outbuf.flush(true).unwrap(), 0); // still not flushed
        // There should be 2 chunks, the first one is full and the second contains 128 words
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.chunks.len(), 2);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1024);
            assert_eq!(outbuf.chunks.back().unwrap().len(), 128);
        } else {
            assert_eq!(outbuf.chunks.len(), 1);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1152);
        }

        outbuf.wait_for_output_event = false; // allow flushes
        assert_eq!(outbuf.flush(true).unwrap(), 1152);
        assert_eq!(outbuf.chunks.len(), 1);

        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.free_chunks.len(), 1);
        } else {
            assert_eq!(outbuf.free_chunks.len(), 0);
        }

        assert_eq!(inbuf.receive().unwrap(), 1024);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[0]);

        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[1]);
        assert!(inbuf.try_pop().is_none()); // no more complete msgs until we receive more data

        assert_eq!(inbuf.receive().unwrap(), 128);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[2]);
        assert!(inbuf.try_pop().is_none());
    }

    #[test]
    fn inout_free_chunks_test1() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Test of the free_chunks list, its ALLOWED_EXCESS_CHUNKS max length, and re-use of chunks

        #[allow(clippy::cast_possible_truncation)]
        let msgs: [Vec<u32>; ALLOWED_EXCESS_CHUNKS + 2] =
            array::from_fn(|i| fake_msg(i as u32, 1024));

        outbuf.wait_for_output_event = true; // prevent flushes
        for m in &msgs {
            assert_eq!(outbuf.send_raw([], m).unwrap(), 0); // not flushed
        }
        assert_eq!(outbuf.chunks.len(), ALLOWED_EXCESS_CHUNKS + 2);
        assert_eq!(outbuf.free_chunks.len(), 0);

        outbuf.wait_for_output_event = false; // allow flushes

        // The amount flushed before any is received could be less on systems with very restrictive
        // socket buffers, but on most linuxes, the standard capacity is 52 * 4K, so not a problem
        // if ALLOWED_EXCESS_CHUNKS isn't too big.
        assert_eq!(outbuf.flush(true).unwrap(), (ALLOWED_EXCESS_CHUNKS + 2) * 1024);
        assert_eq!(outbuf.chunks.len(), 1);
        assert_eq!(outbuf.free_chunks.len(), ALLOWED_EXCESS_CHUNKS);

        for m in &msgs {
            assert_eq!(inbuf.receive().unwrap(), 1024);
            let (inmsg, _) = inbuf.try_pop().unwrap();
            assert_eq!(inmsg, m);
        }

        // repeat so that we reuse the free list:

        outbuf.wait_for_output_event = true; // prevent flushes
        for m in &msgs {
            assert_eq!(outbuf.send_raw([], m).unwrap(), 0); // not flushed
        }
        assert_eq!(outbuf.chunks.len(), ALLOWED_EXCESS_CHUNKS + 2);
        assert_eq!(outbuf.free_chunks.len(), 0);

        outbuf.wait_for_output_event = false; // allow flushes

        assert_eq!(outbuf.flush(true).unwrap(), (ALLOWED_EXCESS_CHUNKS + 2) * 1024);
        assert_eq!(outbuf.chunks.len(), 1);
        assert_eq!(outbuf.free_chunks.len(), ALLOWED_EXCESS_CHUNKS);

        for m in &msgs {
            assert_eq!(inbuf.receive().unwrap(), 1024);
            let (inmsg, _) = inbuf.try_pop().unwrap();
            assert_eq!(inmsg, m);
        }
    }

    #[test]
    fn inout_send_test2() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Like inout_test2, only with send instead of send_raw

        outbuf.wait_for_output_event = true; // prevent flushes

        // test chunk extending for both the fits in remaining capacty and doesn't fit cases:
        #[allow(clippy::cast_possible_truncation)]
        let msgs: [Vec<u32>; 3] = std::array::from_fn(|i| fake_msg_for_send(i as u32, 384));
        for m in &msgs {
            assert_eq!(send_fake_msg_for_send(&mut outbuf, [], m).unwrap(), 0); // not flushed
        }
        assert_eq!(outbuf.flush(true).unwrap(), 0); // still not flushed
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.chunks.len(), 2);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1024);
            assert_eq!(outbuf.chunks.back().unwrap().len(), 128);
        } else {
            assert_eq!(outbuf.chunks.len(), 1);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1152);
        }

        outbuf.wait_for_output_event = false; // allow flushes
        assert_eq!(outbuf.flush(true).unwrap(), 1152);
        assert_eq!(outbuf.chunks.len(), 1);
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.free_chunks.len(), 1);
        } else {
            assert_eq!(outbuf.free_chunks.len(), 0);
        }

        assert_eq!(inbuf.receive().unwrap(), 1024);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[0]);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[1]);
        assert_eq!(inbuf.receive().unwrap(), 128);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[2]);
    }

    #[test]
    fn inout_send_test3() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Like inout_send_test2, but with an additional msg to exercise splitting even in the non
        // LIMIT_SENDS_TO_MAX_WORDS_OUT case.

        outbuf.wait_for_output_event = true; // prevent flushes

        // test chunk extending for both the fits in remaining capacty and doesn't fit cases:
        #[allow(clippy::cast_possible_truncation)]
        let msgs: [Vec<u32>; 4] = std::array::from_fn(|i| fake_msg_for_send(i as u32, 384));
        for m in &msgs {
            assert_eq!(send_fake_msg_for_send(&mut outbuf, [], m).unwrap(), 0); // not flushed
        }
        assert_eq!(outbuf.flush(true).unwrap(), 0); // still not flushed
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.chunks.len(), 2);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1024);
            assert_eq!(outbuf.chunks.back().unwrap().len(), 512);
        } else {
            assert_eq!(outbuf.chunks.len(), 2);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1152);
            assert_eq!(outbuf.chunks.back().unwrap().len(), 384);
        }

        outbuf.wait_for_output_event = false; // allow flushes
        assert_eq!(outbuf.flush(true).unwrap(), 1536);
        assert_eq!(outbuf.chunks.len(), 1);
        assert_eq!(outbuf.free_chunks.len(), 1);

        assert_eq!(inbuf.receive().unwrap(), 1024);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[0]);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[1]);
        assert_eq!(inbuf.receive().unwrap(), 512);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[2]);
        let (inmsg, _) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[3]);
    }

    #[test]
    fn fd_test1() {
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Sanity test for fds - send 2 fds with a tiny msg.

        let msg = fake_msg(1, 2);
        let fd1 = fd_for_test(42);
        let fd2 = fd_for_test(13);

        assert_eq!(outbuf.send_raw([fd1, fd2], &msg).unwrap(), 0); // not flushed
        assert_eq!(outbuf.flush(true).unwrap(), 2);
        assert_eq!(inbuf.receive().unwrap(), 2);

        let (inmsg, fdqueue) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msg);
        assert_eq!(fdqueue.len(), 2);
        let outfd1 = fdqueue.pop_front().unwrap();
        let outfd2 = fdqueue.pop_front().unwrap();
        assert!(check_test_fd(outfd1, 42));
        assert!(check_test_fd(outfd2, 13));
    }

    #[test]
    fn too_many_fds_test1() {
        #![allow(clippy::cast_possible_truncation)]
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);
        // Test the too_many_fds logic.

        outbuf.wait_for_output_event = true; // prevent auto flush

        // MAX_FDS_OUT is 28, but MAX_ARGS is 20 (both are eternally fixed by Wayland spec).  We
        // will use 2 msgs with 20 fds each to force a second chunk.

        let msg1 = fake_msg(1, 2);
        let fd1array: [OwnedFd; 20] = array::from_fn(|i| fd_for_test(i as u32));
        let msg2 = fake_msg(2, 2);
        let fd2array: [OwnedFd; 20] = array::from_fn(|i| fd_for_test(i as u32 + 20));

        assert_eq!(outbuf.send_raw(fd1array, &msg1).unwrap(), 0);
        assert_eq!(outbuf.chunks.len(), 1);
        assert_eq!(outbuf.send_raw(fd2array, &msg2).unwrap(), 0);
        assert_eq!(outbuf.chunks.len(), 2); // fds forced a second chunk
        assert_eq!(outbuf.fds.len(), 40);

        outbuf.wait_for_output_event = false; // open the flood gates
        assert_eq!(outbuf.flush(true).unwrap(), 4);
        assert_eq!(outbuf.chunks.len(), 1);
        assert_eq!(outbuf.chunks.front().unwrap().len(), 0);
        assert_eq!(outbuf.fds.len(), 0);

        // We will only get the first msg, even though both were flushed together.  Why?  It's not
        // something we did.  I suspect that if we arranged to receive all 40 fds, then we would
        // have received both msgs.  But the recvmsg call (the kernel socket internal logic) must be
        // smart enough to know that having fds but no data is not a good condition for it to be in,
        // so it prevents it.  What the kernel must be doing is associating a msg length with a
        // number of fds - and if the receiver doesn't have room for all of the fds, it won't
        // receive the associated msg in that recvmsg call.  Note that this is a slightly different
        // problem from the too_many_fds case we have to handle, which prevents fds from not being
        // sendable because we have a send max of MAX_FDS_OUT.  Having fds we can't send can always
        // starve the receiver regardless of the measures that recvmsg takes.
        assert_eq!(inbuf.receive().unwrap(), 2);

        let (inmsg1, fdqueue1) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg1, msg1);
        // We should get 28 fds with this, even though our first send had 20 fds.  The kernel will
        // allow recvmsg to accept more but not less fds than were sent with a chunk.  Which is the
        // right thing to do, because less could starve the receiver, as there is no way to recvmsg
        // only fds when there is no accompanying data.
        assert_eq!(fdqueue1.len(), 28);
        for i in 0..28 {
            let fd = fdqueue1.pop_front().unwrap();
            assert!(check_test_fd(fd, i));
        }
        assert!(inbuf.try_pop().is_none());

        assert_eq!(inbuf.receive().unwrap(), 2);
        let (inmsg2, fdqueue2) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg2, msg2);
        // Now we get the remaining 12 fds
        assert_eq!(fdqueue2.len(), 12);
        for i in 28..40 {
            let fd = fdqueue2.pop_front().unwrap();
            assert!(check_test_fd(fd, i));
        }
    }

    #[test]
    fn inout_send_with_fd_test() {
        // About LIMIT_SENDS_TO_MAX_WORDS_OUT - is this needed for some peers?  In other words, is
        // it possible that sending more than 4Kbytes (MAX_WORDS_OUT * 4) per flush will cause
        // problems with the receiver?  The inout_send_with_fd_test attempts to address this.  It
        // shows that if LIMIT_SENDS_TO_MAX_WORDS_OUT is false and we flush more than 4Kbytes, and
        // there are some fds as well (the test only has one, but that's enough to demonstrate
        // this), then the receiver will receive the fd with the first recvmsg call, even though
        // that call doesn't have enough room for all of the flushed data.
        let (s1, s2) = UnixStream::pair().unwrap();
        s1.set_nonblocking(true).unwrap();
        s2.set_nonblocking(true).unwrap();
        let mut inbuf = InBuffer::new(&s1);
        let mut outbuf = OutBuffer::new(&s2);

        outbuf.wait_for_output_event = true; // prevent flushes

        // test chunk extending for both the fits in remaining capacty and doesn't fit cases:
        #[allow(clippy::cast_possible_truncation)]
        let msgs: [Vec<u32>; 3] = std::array::from_fn(|i| fake_msg_for_send(i as u32, 384));
        let fd = fd_for_test(1);

        assert_eq!(send_fake_msg_for_send(&mut outbuf, [fd], &msgs[0]).unwrap(), 0);
        assert_eq!(send_fake_msg_for_send(&mut outbuf, [], &msgs[1]).unwrap(), 0);
        assert_eq!(send_fake_msg_for_send(&mut outbuf, [], &msgs[2]).unwrap(), 0);
        assert_eq!(outbuf.flush(true).unwrap(), 0); // still not flushed

        assert_eq!(outbuf.fds.len(), 1);
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.chunks.len(), 2);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1024);
            assert_eq!(outbuf.chunks.back().unwrap().len(), 128);
        } else {
            assert_eq!(outbuf.chunks.len(), 1);
            assert_eq!(outbuf.chunks.front().unwrap().len(), 1152);
        }

        outbuf.wait_for_output_event = false; // allow flushes
        assert_eq!(outbuf.flush(true).unwrap(), 1152);
        assert_eq!(outbuf.chunks.len(), 1);
        if LIMIT_SENDS_TO_MAX_WORDS_OUT {
            assert_eq!(outbuf.free_chunks.len(), 1);
        } else {
            assert_eq!(outbuf.free_chunks.len(), 0);
        }

        assert_eq!(inbuf.receive().unwrap(), 1024);
        let (inmsg, fdqueue) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[0]);
        assert_eq!(fdqueue.len(), 1);
        assert!(check_test_fd(fdqueue.pop_front().unwrap(), 1));
        let (inmsg, fdqueue) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[1]);
        assert!(fdqueue.is_empty());
        assert_eq!(inbuf.receive().unwrap(), 128);
        let (inmsg, fdqueue) = inbuf.try_pop().unwrap();
        assert_eq!(inmsg, msgs[2]);
        assert!(fdqueue.is_empty());
    }

    #[test]
    fn test_extend_chunk() {
        // exercise ExtendChunk fully
        let mut chunk = Chunk::new();
        let mut extend = ExtendChunk(&mut chunk);

        extend.add_u32(42);
        extend.add_i32(13);
        extend.add_array(&[]);
        extend.add_array(&[0x01]);
        extend.add_array(&[0x23, 0x45]);
        extend.add_array(&[0x67, 0x89, 0xab]);
        extend.add_array(&[0xcd, 0xef, 0xfe, 0xdc]);
        extend.add_array(&[0xba, 0x98, 0x76, 0x54, 0x32, 0x10]);
        extend.add_array(b"abcdefghijklmnopqrstuvwxyz");
        let should_be = [
            42u32.to_ne_bytes(),
            13i32.to_ne_bytes(),
            0u32.to_ne_bytes(),
            1u32.to_ne_bytes(),
            [0x01, 0, 0, 0],
            2u32.to_ne_bytes(),
            [0x23, 0x45, 0, 0],
            3u32.to_ne_bytes(),
            [0x67, 0x89, 0xab, 0],
            4u32.to_ne_bytes(),
            [0xcd, 0xef, 0xfe, 0xdc],
            6u32.to_ne_bytes(),
            [0xba, 0x98, 0x76, 0x54],
            [0x32, 0x10, 0, 0],
            26u32.to_ne_bytes(),
            *b"abcd",
            *b"efgh",
            *b"ijkl",
            *b"mnop",
            *b"qrst",
            *b"uvwx",
            *b"yz\0\0",
        ]
        .concat();
        assert_eq!(to_u8_slice(&chunk), &should_be[..]);
    }
}
