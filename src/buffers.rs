#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, OwnedFd};

use crate::input_handler::{OutChannel, PopFds};
use crate::streams::{InStream, OutStream, MAX_FDS_OUT};
use arrayvec::ArrayVec;
use std::collections::{LinkedList, VecDeque};

const MAX_BYTES_OUT: usize = 4096;

type Chunk = ArrayVec<u8, MAX_BYTES_OUT>;
// Either of the following works:
type Chunks = LinkedList<Chunk>;
//type Chunks = VecDeque<Box<Chunk>>;

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
        Self {
            data: [0u8; MAX_BYTES_OUT * 2],
            front: 0,
            back: 0,
            fds: VecDeque::with_capacity(MAX_FDS_OUT),
        }
    }

    // pop a single message from the buffer, based on the length in its header
    pub(crate) fn try_pop(&mut self) -> Option<(&[u8], &mut impl PopFds)> {
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

#[derive(Debug)]
pub(crate) struct OutBuffer<S: OutStream> {
    chunks: Chunks,
    free_chunks: Chunks,
    fds: VecDeque<OwnedFd>,
    pub(crate) flush_every_send: bool,
    stream: S,
}

impl<S: OutStream> OutBuffer<S> {
    #![allow(clippy::default_trait_access)]
    pub(crate) fn new(stream: S) -> Self {
        Self {
            chunks: Default::default(),
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
        if self.free_chunks.is_empty() {
            self.chunks.push_back(Default::default());
        } else {
            let rest = self.free_chunks.split_off(1);
            debug_assert_eq!(self.free_chunks.len(), 1);
            self.chunks.append(&mut self.free_chunks);
            self.free_chunks = rest;
        }
    }

    fn is_empty(&self) -> bool {
        #![allow(dead_code)]
        self.chunks.front().expect(MUST_HAVE1).len() == 0
    }

    fn end_mut(&mut self) -> &mut Chunk {
        self.chunks.back_mut().expect(MUST_HAVE1)
    }

    fn too_many_fds(&self) -> bool {
        self.fds.len() > self.chunks.len() * MAX_FDS_OUT
    }

    // By not owning the stream, we will need to have the caller of push have access to the mut
    // stream in order for it to get down to flush_first_chunk.  Which won't work out well.
    // Maybe what we need to do is combine InBuffer and OutBuffer together.  They can't be
    // separate objects, and the one combined object needs to own the stream.  This reflects on
    // the difficulty of properly modularizing Rust in the presence of its anti-aliasing rules.
    // We need multiple places to have access to a &mut stream, but they can't all unless they
    // can all access the stream's owner, but we want that to be their owner.  Alternatively,
    // maybe we want send and receive to be no mut methods, and that they get the cmsg_space mut
    // ref from 2 distinct cmsg_space owners.  Maybe we make cmsg_space thread-local?  Then we
    // use a thread_local! RefCell and use it with with_borrow_mut.  That will test if there
    // already is a borrow, which is a waste.  But there is no way around that check if we use
    // thread-local, even if we move instead of borrow.
    //
    // Also, if we try to have 2 shared refs to the stream that are themselves owned by the
    // bufs, we can't have anything mutable in the owner of the stream easily.  I think this
    // dictates that we have a single inout buffer that owns the stream.  Although we can split
    // the externally accessible interface into 2 separate traits.
    //
    // But note that recvmsg takes an AsFd, and so does sendmsg.  But that still means borrowing
    // from a common OwnedFd.

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

#[allow(clippy::inline_always)]
#[inline(always)]
const fn round4(x: usize) -> usize {
    (x + 3) & !3
}

impl<S: OutStream> OutChannel for OutBuffer<S> {
    // for each msg, push fds first, then push the msg.  This is because we want any fds to be
    // associated with the earliest possible data chunk to help prevent fd starvation of the
    // receiver.
    fn push_fd(&mut self, fd: OwnedFd) -> IoResult<usize> {
        self.fds.push_back(fd);
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
        Ok(0)
    }

    fn push_u32(&mut self, data: u32) -> IoResult<usize> {
        let len = std::mem::size_of::<u32>();
        let end_chunk = self.end_mut();
        let split_point = end_chunk.remaining_capacity();
        if split_point >= len {
            // all of msg fits in end chunk
            let r = end_chunk.try_extend_from_slice(&data.to_ne_bytes());
            debug_assert!(r.is_ok());
        } else {
            self.start_next_chunk();
            // write rest of msg
            let end_chunk = self.end_mut(); // need recalc due to start_next_chunk
            debug_assert!(end_chunk.is_empty());
            let r = end_chunk.try_extend_from_slice(&data.to_ne_bytes());
            debug_assert!(r.is_ok());
        }
        Ok(len)
    }

    fn push_array(&mut self, data: &[u8]) -> IoResult<usize> {
        // push len first
        let len = data.len();
        // The length pushed here is the length of the string/array without padding or len header:
        #[allow(clippy::cast_possible_truncation)]
        self.push_u32(len as u32)?;
        let len4 = round4(len);
        let mut end_chunk = self.end_mut();
        let split_point = end_chunk.remaining_capacity();
        if split_point < len4 {
            // we don't have to finish the last chunk to start the next
            self.start_next_chunk();
            end_chunk = self.end_mut();
        }
        let r = end_chunk.try_extend_from_slice(data);
        debug_assert!(r.is_ok());
        let pad = len4 - len;
        if pad > 0 {
            let r = end_chunk.try_extend_from_slice(&vec![0; pad]);
            debug_assert!(r.is_ok());
        }
        Ok(len4 + std::mem::size_of::<u32>())
    }

    fn push(&mut self, msg: &[u8]) -> IoResult<usize> {
        let end_chunk = self.end_mut();
        let split_point = end_chunk.remaining_capacity();
        let msg_len = msg.len();
        debug_assert!(msg_len <= MAX_BYTES_OUT);
        if split_point >= msg_len {
            // all of msg fits in end chunk
            let r = end_chunk.try_extend_from_slice(msg);
            debug_assert!(r.is_ok());
        } else {
            if split_point > 0 {
                // some of msg fits in end chunk
                let r = end_chunk.try_extend_from_slice(&msg[..split_point]);
                debug_assert!(r.is_ok());
            }
            self.start_next_chunk();
            // write rest of msg
            let end_chunk = self.end_mut(); // need recalc due to start_next_chunk
            debug_assert!(end_chunk.is_empty());
            let r = end_chunk.try_extend_from_slice(&msg[split_point..]);
            debug_assert!(r.is_ok());
        }
        Ok(msg_len)
    }

    fn end_msg(&mut self) -> IoResult<usize> {
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

fn get_msg_length(header: &[u8]) -> Option<usize> {
    let word_2 = u32::from_ne_bytes(header.get(4..8)?.try_into().ok()?);
    Some((word_2 >> 16) as usize)
}

const MUST_HAVE1: &str = "active_chunks must always have at least 1 element";
const ALLOWED_EXCESS_CHUNKS: usize = 8;
