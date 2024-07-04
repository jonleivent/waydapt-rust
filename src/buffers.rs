#![warn(clippy::pedantic)]
//#![forbid(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, OwnedFd};

use crate::basics::{get_msg_length, init_array, MAX_BYTES_OUT, MAX_FDS_OUT};
use crate::crate_traits::{FdInput, InStream, MessageSender, OutStream};
use arrayvec::ArrayVec;
use std::collections::{LinkedList, VecDeque};

#[derive(Debug)]
pub(crate) struct InBuffer {
    data: [u8; MAX_BYTES_OUT * 2],
    front: usize,
    back: usize,
    fds: VecDeque<OwnedFd>,
}

impl Default for InBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InBuffer {
    pub(crate) fn new() -> Self {
        Self { data: init_array(0), front: 0, back: 0, fds: VecDeque::with_capacity(MAX_FDS_OUT) }
    }

    // pop a single message from the buffer, based on the length in its header
    pub(crate) fn try_pop(&mut self) -> Option<(&[u8], &mut impl FdInput)> {
        let available = &self.data[self.front..self.back];
        let msg_len = get_msg_length(available)?;
        let msg = available.get(0..msg_len)?;
        self.front += msg_len;
        Some((msg, &mut self.fds))
    }

    pub(crate) fn receive<S>(&mut self, stream: &'_ mut S) -> IoResult<usize>
    where
        S: InStream,
    {
        self.compact();
        debug_assert!(self.back <= MAX_BYTES_OUT);
        // if we have more than MAX_BYTES_OUT available storage, should we allow receive to use
        // it?  That might cause problems with fds - if we receive too much, we might leave fds
        // in the kernel socket buffer that we need.  How would this happen?  Suppose that the
        // kernel socket buffer contains MAX_BYTES_OUT + D data and MAX_FDS_OUT + F fds, and we
        // have room for all MAX_BYTES_OUT + D data.  We will only take in MAX_FDS_OUT fds,
        // however, and if the first MAX_BYTES_OUT msgs use all MAX_FDS_OUT fds, then the last D
        // msgs will be starved of fds.  So we have to limit the message input to MAX_BYTES_OUT.
        let buf = &mut self.data[self.back..(MAX_BYTES_OUT + self.back)];
        let amount_read = stream.receive(buf, &mut self.fds)?;
        self.back += amount_read;
        Ok(amount_read)
    }

    fn compact(&mut self) {
        let len = self.back - self.front;
        debug_assert!(len <= MAX_BYTES_OUT);
        // Compacting more often than absolutely necessary has a cost, but it may improve CPU
        // cache friendliness.
        let threshold = match COMPACT_SCHEME {
            CompactScheme::Eager => len,
            CompactScheme::Lazy => MAX_BYTES_OUT + 1,
        };
        if self.front >= threshold {
            // the total current payload fits below front, so move it there.  We can use the
            // memcpy-based copy_from_slice instead of the memmove-based copy_within because of
            // the extra room.  Doing so may get us some vectorization speedup.
            let (left, right) = self.data.split_at_mut(self.front);
            left[..len].copy_from_slice(&right[..len]);
            self.back = len;
            self.front = 0;
        }
    }
}

const CHUNK_SIZE: usize = MAX_BYTES_OUT * 2;

type Chunk = ArrayVec<u8, CHUNK_SIZE>;
// Either of the following works:
type Chunks = LinkedList<Chunk>;
//type Chunks = VecDeque<Box<Chunk>>;

#[derive(Debug)]
pub(crate) struct OutBuffer<S: OutStream> {
    chunks: Chunks,
    free_chunks: Chunks,
    fds: VecDeque<OwnedFd>,
    pub(crate) flush_every_send: bool,
    stream: S,
}

fn initial_chunks() -> Chunks {
    // Chunks with a single empty Chunk
    let mut new: Chunks = Default::default();
    new.push_back(Default::default());
    new
}

const LIMIT_SENDS_TO_MAX_BYTES_OUT: bool = false;

impl<S: OutStream> OutBuffer<S> {
    #![allow(clippy::default_trait_access)]
    pub(crate) fn new(stream: S) -> Self {
        Self {
            chunks: initial_chunks(),
            free_chunks: Default::default(),
            fds: VecDeque::with_capacity(MAX_FDS_OUT),
            flush_every_send: false,
            stream,
        }
    }

    pub(crate) fn get_stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub(crate) fn get_stream(&self) -> &S {
        &self.stream
    }

    fn start_next_chunk(&mut self) {
        let mut new_end: Chunks = if self.free_chunks.is_empty() {
            initial_chunks()
        } else {
            self.free_chunks.split_off(self.free_chunks.len() - 1)
        };
        assert!(new_end.front().unwrap().is_empty());
        if LIMIT_SENDS_TO_MAX_BYTES_OUT && self.end().len() > MAX_BYTES_OUT {
            let old_end = self.end_mut();
            let overflow = &old_end[MAX_BYTES_OUT..];
            new_end.front_mut().unwrap().extend(overflow.iter().copied());
            old_end.truncate(MAX_BYTES_OUT);
        }
        self.chunks.append(&mut new_end);
    }

    fn is_empty(&self) -> bool {
        #![allow(dead_code)]
        self.chunks.front().expect(MUST_HAVE1).len() == 0
    }

    fn end(&self) -> &Chunk {
        self.chunks.back().expect(MUST_HAVE1)
    }

    fn end_mut(&mut self) -> &mut Chunk {
        self.chunks.back_mut().expect(MUST_HAVE1)
    }

    fn too_many_fds(&self) -> bool {
        self.fds.len() > self.chunks.len() * MAX_FDS_OUT
    }

    pub(crate) fn flush(&mut self, force: bool) -> IoResult<usize> {
        let mut total_flushed = 0;
        // self.active_chunks.len() > 1 means that there are chunks waiting to get flushed that
        // were refused prevoiusly because the kernel socket buffer did not have room - those
        // should always get flushed if there is now room.  The final chunk should only get
        // flushed after all pending message traffic has been processed - meaning just before
        // starting what might be a traffic lull.  Or when we get signalled that there's now
        // room in the kernel socket buffer for messages we tried to flush previously.
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
        if first_chunk.is_empty() {
            return Ok(0);
        }
        let nfds = std::cmp::min(self.fds.len(), MAX_FDS_OUT);
        let amount_flushed = if nfds > 0 {
            let mut bfds = ArrayVec::<_, MAX_FDS_OUT>::new(); // or Vec::with_capacity(nfds)
            bfds.extend(self.fds.iter().take(nfds).map(OwnedFd::as_fd));
            self.stream.send(first_chunk, &bfds)?
        } else {
            self.stream.send(first_chunk, &[])?
        };
        // if amount_flushed == 0, then not even the fds were flushed
        if amount_flushed > 0 {
            // If the flush happened, then we expect the whole chunk was taken:
            debug_assert_eq!(amount_flushed, first_chunk.len());
            // remove flushed fds, which will close the corresponding files:
            self.fds.drain(..nfds);
            // remove flushed msg data:
            first_chunk.clear();
            if self.chunks.len() > 1 {
                // there are other active chunks, so first_chunk needs to be popped and possibly
                // moved to inactive_chunks
                let rest_active_chunks = self.chunks.split_off(1);
                debug_assert_eq!(self.chunks.len(), 1);
                // if we are below the excess threshold, keep it, else let it drop
                if self.free_chunks.len() < ALLOWED_EXCESS_CHUNKS {
                    self.free_chunks.append(&mut self.chunks);
                }
                self.chunks = rest_active_chunks;
                debug_assert!(!self.chunks.is_empty());
            }
        }
        Ok(amount_flushed)
    }
}

impl<S: OutStream> MessageSender for OutBuffer<S> {
    fn send(
        &mut self, fds: impl Iterator<Item = OwnedFd>, msgfun: impl FnOnce(&mut [u8]) -> usize,
    ) -> IoResult<usize> {
        // for each msg, push fds first, then push the msg.  This is because we want any fds to be
        // associated with the earliest possible data chunk to help prevent fd starvation of the
        // receiver.
        self.fds.extend(fds);
        // Prevent the possibility of having fds in the OutBuffer but no msg data, which can
        // cause starvation of the receiver.  This can happen whenever there are more fds than
        // can be flushed with the existing data, due to the MAX_FDS_OUT limit.
        if self.too_many_fds() {
            self.flush(true)?;
            if self.too_many_fds() {
                self.start_next_chunk();
                debug_assert!(!self.too_many_fds());
            }
        }
        // Is there enough space in the end chunk for a max-sized message?  If not, start a new chunk:
        if self.end().remaining_capacity() < MAX_BYTES_OUT {
            self.start_next_chunk();
        };
        // Unsafely give msgfun a mut slice large enough for the max message:
        let end_chunk = self.end_mut();
        let orig_len = end_chunk.len();
        // Why this unsafe block isn't that unsafe: the elements of a chunk are u8s, set_len doesn't
        // allow us to set the length beyond the capacity of the ArrayVec, and we will reset the
        // length below to its proper value (assuming msgfun returned the proper value, which we
        // check with a debug_assert):
        unsafe { end_chunk.set_len(orig_len + MAX_BYTES_OUT) };
        let len = msgfun(&mut end_chunk[orig_len..]);
        debug_assert!(len >= 8 && len <= MAX_BYTES_OUT);
        debug_assert_eq!(len, get_msg_length(end_chunk).unwrap());
        // .. then set the length acurately:
        end_chunk.truncate(orig_len + len);

        // This flush is not forced (unless flush_every_send is true) because somewhere up the
        // call chain there is someone responsible for forcing a flush after all pending message
        // traffic has been processed.  This flush is only for excess (after the first chunk),
        // and is not necessary.  It's benefit is that it may raise the throughput by allowing
        // more parallelism between sender and receiver.  It may also limit the number of chunks
        // needed in this OutBuffer.
        self.flush(self.flush_every_send)
    }
}

pub enum CompactScheme {
    Eager,
    Lazy,
}

const COMPACT_SCHEME: CompactScheme = CompactScheme::Eager;

const MUST_HAVE1: &str = "active_chunks must always have at least 1 element";
const ALLOWED_EXCESS_CHUNKS: usize = 8;
