#![allow(unused)]
#![forbid(unsafe_code)]

use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

const MAX_FDS_OUT: usize = 28;
const MAX_BYTES_OUT: usize = 4096;
const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_OUT));

mod event_loop {
    use epoll::{CreateFlags, Event, EventData, EventFlags, EventVec};
    use rustix::event::epoll;
    use std::io::Result as IoResult;
    use std::os::fd::BorrowedFd;

    pub(crate) trait EventLoop {
        fn in_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>>;
        fn out_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>>;
        fn handle_input(&mut self, fd_index: usize) -> IoResult<()>;
        fn handle_output(&mut self, fd_index: usize) -> IoResult<()>;
        const EDGE_TRIGGER_INPUT: bool = true;

        fn handle_error(&mut self, fd_index: usize, nin: usize, flags: EventFlags) -> IoResult<()> {
            use std::io::{Error, ErrorKind};
            // TBD - maybe improve error messages for some sets of flags
            Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("event flags: {:?}", flags.iter_names().collect::<Vec<_>>()),
            ))
        }

        // The rest of these shouldn't be overridden.  Should we move them out of the trait?

        fn handle(&mut self, fd_index: usize, nin: usize, flags: EventFlags) -> IoResult<()> {
            if fd_index < nin {
                // we only expect input
                if flags.contains(EventFlags::IN) {
                    self.handle_input(fd_index)?;
                }
                if !flags.difference(EventFlags::IN).is_empty() {
                    self.handle_error(fd_index, nin, flags)?;
                }
            } else {
                // we only expect output
                if flags.contains(EventFlags::OUT) {
                    self.handle_output(fd_index - nin)?;
                }
                if !flags.difference(EventFlags::OUT).is_empty() {
                    self.handle_error(fd_index, nin, flags)?;
                }
            }
            Ok(())
        }

        fn event_loop(&mut self) -> IoResult<()> {
            let epoll_fd = epoll::create(CreateFlags::CLOEXEC)?;
            let in_fds = self.in_fds();
            let mut i = 0;
            let in_flags = if Self::EDGE_TRIGGER_INPUT {
                EventFlags::IN | EventFlags::ET
            } else {
                EventFlags::IN
            };
            for fd in in_fds {
                let data = EventData::new_u64(i);
                epoll::add(&epoll_fd, fd, data, in_flags);
                i += 1;
            }
            let nin = i as usize;
            let out_fds = self.out_fds();
            // output has to be edge triggered (ET), because otherwise we would get an event
            // whenever there is room in the kernel socket buffer
            let out_flags = EventFlags::OUT | EventFlags::ET;
            for fd in out_fds {
                let data = EventData::new_u64(i);
                epoll::add(&epoll_fd, fd, data, out_flags);
                i += 1;
            }
            let mut events = EventVec::with_capacity(i as usize); // would longer help?
            loop {
                epoll::wait(&epoll_fd, &mut events, 0)?;
                if events.is_empty() {
                    // should never occur with 0 timeout
                    panic!("Got 0 events from epoll::wait");
                }
                for Event { flags, data } in events.iter() {
                    let i = data.u64() as usize;
                    self.handle(i, nin, flags)?;
                }
                events.clear(); // may not be needed
            }
        }
    }
}

// If we wanted to single-thread waydapt, we would put everything including the wayland socket fd
// into the same event loop above.  The wayland socket fd, which only handles input, would get some
// distinguished data index like ~0 - and that would be hard coded into the event loop.  The 4 fds
// of each session would then be at indexes that differ only by the low 2 bits.  If the handle_all
// on any fd in a session generates an error, it would be handled in event_loop, and all 4 fds would
// get deleted from the epoll.  However, an issue we can't handle in the single-thread case is
// fairness - one client could starve out all of the others, even when handling only a single
// message, if that message takes a long time to handle.

// There's probably no benefit in a truly single threaded waydapt.  The thread that receives wayland
// connections does not share any data with the actual work threads.  So maybe the minimal config is
// 2 threads - one for wayland connections, the other for all other work.  In which case the above
// epoll doesn't need to deal with the wayland connection fd at all, and can consider all fd paired,
// or actually quaded.

// If we have multiple sessions in a single event loop - meaning we go with the 2 thread model where
// all sessions are in a single thread (and the other is for wayland socket connects), then how do
// we add/remove sessions?  That would be very difficult to accomplish without sharing the epoll fd
// with the wayland socket thread, so that it can add new sessions.

// Also, there is no Rust borrow-based guarantee that a OwnedFd won't be dropped while it is being
// polled, so we have to be careful somehow.  Maybe this suggests that it isn't worth trying to run
// multiple sessions in the same event loop, because then we need a way to shut down parts of the
// event loop without other parts.

mod input_buffer {
    use super::*;
    use std::collections::VecDeque;
    use std::io::empty;
    use std::marker::PhantomData;

    pub(crate) trait InStream {
        fn receive(&mut self, buf: &mut [u8], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize>;
    }

    #[derive(Debug)]
    pub(crate) struct InUnixStream {
        stream: UnixStream,
        cmsg_space: [u8; CMSG_SPACE],
    }

    impl InUnixStream {
        pub(crate) fn new(stream: UnixStream) -> Self {
            Self { stream, cmsg_space: [0u8; CMSG_SPACE] }
        }
    }

    impl AsFd for InUnixStream {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    #[cfg(target_os = "macos")]
    fn cloexec_fd(fd: Fd) {
        use rustix::io;
        if let Ok(flags) = io::fcntl_getfd(fd) {
            let _ = io::fcntl_setfd(fd, flags | io::FdFlags::CLOEXEC);
        }
    }

    impl InStream for InUnixStream {
        fn receive(&mut self, buf: &mut [u8], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize> {
            use rustix::io::{retry_on_intr, Errno, IoSliceMut};
            use rustix::net::{recvmsg, RecvFlags, RecvMsgReturn};
            use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage};

            let flags = RecvFlags::DONTWAIT;
            #[cfg(not(target_os = "macos"))]
            let flags = flags | RecvFlags::CMSG_CLOEXEC;

            let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut self.cmsg_space);
            let mut iov = [IoSliceMut::new(buf)];
            let recv = || recvmsg(&self.stream, &mut iov, &mut cmsg_buffer, flags);
            let bytes = match retry_on_intr(recv) {
                Ok(b) => b.bytes,
                Err(Errno::WOULDBLOCK) | Err(Errno::AGAIN) => 0,
                Err(e) => return Err(e.into()),
            };
            if bytes > 0 {
                let only_fds = |cmsg| match cmsg {
                    RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                    _ => None,
                };
                let received_fds = cmsg_buffer.drain().filter_map(only_fds).flatten();

                #[cfg(target_os = "macos")]
                let received_fds = received_fds.map(cloexec_fd);

                fds.extend(received_fds);
            }
            Ok(bytes)
        }
    }

    pub enum CompactScheme {
        Eager,
        Lazy,
    }

    pub(crate) trait InBufferConfig {
        const COMPACT_SCHEME: CompactScheme;
        fn get_msg_length(header: &[u8]) -> Option<usize>;
        type Stream: InStream + AsFd;
    }

    #[derive(Debug)]
    pub(crate) struct InBuffer<Config: InBufferConfig> {
        storage: [u8; MAX_BYTES_OUT * 2], // *2 on purpose!
        front: usize,
        back: usize,
        fds: VecDeque<OwnedFd>,
        stream: Config::Stream,
        _pd: PhantomData<Config>,
    }

    impl<Config: InBufferConfig> AsFd for InBuffer<Config> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    pub(crate) trait PopFds {
        fn pop(&mut self) -> Option<OwnedFd>;
    }

    impl PopFds for VecDeque<OwnedFd> {
        fn pop(&mut self) -> Option<OwnedFd> {
            self.pop_front()
        }
    }

    impl<Config: InBufferConfig> InBuffer<Config> {
        pub(crate) fn new(stream: Config::Stream) -> Self {
            Self {
                storage: [0u8; MAX_BYTES_OUT * 2], // *2 on purpose!
                front: 0,
                back: 0,
                fds: VecDeque::with_capacity(MAX_FDS_OUT),
                stream,
                _pd: PhantomData,
            }
        }

        // pop a single message from the buffer, based on the length in its header
        pub(crate) fn try_pop(&mut self) -> Option<(&[u8], &mut impl PopFds)> {
            let available = &self.storage[self.front..self.back];
            let msg_len = Config::get_msg_length(available)?;
            let msg = available.get(0..msg_len)?;
            self.front += msg_len;
            Some((msg, &mut self.fds))
        }

        pub(crate) fn receive(&mut self) -> IoResult<usize> {
            self.compact();
            debug_assert!(self.back <= MAX_BYTES_OUT);
            // if we have more than MAX_BYTES_OUT available storage, should we allow receive to use
            // it?  That might cause problems with fds - if we receive too much, we might leave fds
            // in the kernel socket buffer that we need.  How would this happen?  Suppose that the
            // kernel socket buffer contains MAX_BYTES_OUT + D data and MAX_FDS_OUT + F fds, and we
            // have room for all MAX_BYTES_OUT + D data.  We will only take in MAX_FDS_OUT fds,
            // however, and if the first MAX_BYTES_OUT msgs use all MAX_FDS_OUT fds, then the last D
            // msgs will be starved of fds.  So we have to limit the message input to MAX_BYTES_OUT.
            let buf = &mut self.storage[self.back..(MAX_BYTES_OUT + self.back)];
            let amount_read = self.stream.receive(buf, &mut self.fds)?;
            self.back += amount_read;
            Ok(amount_read)
        }

        fn compact(&mut self) {
            let len = self.back - self.front;
            debug_assert!(len <= MAX_BYTES_OUT);
            // Compacting more often than absolutely necessary has a cost, but it may improve CPU
            // cache friendliness.
            let threshold = match Config::COMPACT_SCHEME {
                CompactScheme::Eager => len,
                CompactScheme::Lazy => MAX_BYTES_OUT + 1,
            };
            if self.front >= threshold {
                // the total current payload fits below front, so move it there.  We can use the
                // memcpy-based copy_from_slice instead of the memmove-based copy_within because of
                // the extra room.  Doing so may get us some vectorization speedup.
                let (left, right) = self.storage.split_at_mut(self.front);
                left[..len].copy_from_slice(&right[..len]);
                self.back = len;
                self.front = 0;
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct WaydaptInBufferConfig;

    impl InBufferConfig for WaydaptInBufferConfig {
        const COMPACT_SCHEME: CompactScheme = CompactScheme::Eager;
        fn get_msg_length(header: &[u8]) -> Option<usize> {
            // return 0 if not enough room for the header
            let word_2 = u32::from_ne_bytes(header.get(4..8)?.try_into().ok()?);
            Some((word_2 >> 16) as usize)
        }

        type Stream = InUnixStream;
    }
}

////////////////////////////////////////////////////////////////////////////////

mod output_buffer {
    use super::*;
    use arrayvec::ArrayVec;
    use std::collections::{LinkedList, VecDeque};

    pub(crate) trait OutStream {
        fn send(&mut self, data: &[u8], fds: &[BorrowedFd<'_>]) -> IoResult<usize>;
    }

    #[derive(Debug)]
    pub(crate) struct OutUnixStream {
        stream: UnixStream,
        cmsg_space: [u8; CMSG_SPACE],
    }

    impl OutUnixStream {
        pub(crate) fn new(stream: UnixStream) -> Self {
            Self { stream, cmsg_space: [0u8; CMSG_SPACE] }
        }
    }

    impl AsFd for OutUnixStream {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    impl OutStream for OutUnixStream {
        fn send(&mut self, data: &[u8], fds: &[BorrowedFd<'_>]) -> IoResult<usize> {
            use rustix::io::{retry_on_intr, Errno, IoSlice, Result};
            use rustix::net::{send, sendmsg, SendFlags};
            use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};

            let flags = SendFlags::DONTWAIT;
            #[cfg(not(target_os = "macos"))]
            let flags = flags | SendFlags::NOSIGNAL;

            let outfd = self.stream.as_fd();
            let result = if !fds.is_empty() {
                debug_assert!(fds.len() <= MAX_FDS_OUT);
                let iov = [IoSlice::new(data)];
                let mut cmsg_buffer = SendAncillaryBuffer::new(&mut self.cmsg_space);
                let pushed = cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds));
                debug_assert!(pushed);
                retry_on_intr(|| sendmsg(outfd, &iov, &mut cmsg_buffer, flags))
            } else {
                retry_on_intr(|| send(outfd, data, flags))
            };
            match result {
                Ok(b) => Ok(b),
                Err(Errno::WOULDBLOCK) | Err(Errno::AGAIN) => Ok(0),
                Err(e) => Err(e.into()),
            }
        }
    }

    pub(crate) trait OutBufferConfig {
        const ALLOWED_EXCESS_CHUNKS: usize;
        type Stream: OutStream + AsFd;
    }

    type Chunk = ArrayVec<u8, MAX_BYTES_OUT>;
    // Either of the following works:
    type Chunks = LinkedList<Chunk>;
    //type Chunks = VecDeque<Box<Chunk>>;

    const MUST_HAVE1: &str = "active_chunks must always have at least 1 element";

    #[derive(Debug)]
    pub(crate) struct OutBuffer<Config: OutBufferConfig> {
        // Don't use ArrayVec as the chunk type, because chunks may be copied when the VecDeque is
        // reallocated due to growth.  Maybe we should use a linked list instead?  Unfortunately,
        // there is no rotate method for link list.  And popping/pushing will copy the ArrayVec.
        // And even if there were a rotate, we'd need to keep the end pointer into the list somehow.
        // There would need to be a rotate involving 2 lists.  The only problems with
        // VecDeque<Vec<u8>> are that growing the VecDeque may do a realloc, and that accessing Vecs
        // require extra derefs vs ArrayVec.  Ideally, there would be a LinkedList that supported
        // rotating elements off of one list onto another without copying - then we could implement
        // this by splitting chunks into 2 lists active_chunks and inactive_chunks, where
        // active_chunks.front and .back are O(1) accessible.  We can use LinkedList::split_off and
        // append.

        // Does it make more sense to carry the fds in the chunks?  It can be an ArrayVec with max
        // MAX_FDS_OUT capacity.  It probably wastes more space in that case, however.
        active_chunks: Chunks,
        inactive_chunks: Chunks,
        fds: VecDeque<OwnedFd>,
        stream: Config::Stream,
    }

    // The interaction between page data and fds is very easy to get wrong.  It is OK to send fds
    // before they are needed, but not OK to send them later than when they are needed, as the
    // recvmsg system call cannot receive fds without also receiving some message data.  Similarly,
    // the send system call cannot send fds without also sending some message data.  This interacts
    // with the fact that we can only transfer max MAX_FDS_OUT fds per send or recvmsg call.  The
    // state to stay away from on the sending side is then when there are fds in the output buffer
    // but no message data to send them with.  The state to stay away from on the receiving side is
    // having fewer fds in the input buffer than needed by messages in the input buffer.  Hence,
    // always try sending fds as early as possible up to the MAX_FDS_OUT limit, and when that limit
    // is reached, try to flush only the accompanying message data (not more!) together with those
    // fds.

    impl<Config: OutBufferConfig> AsFd for OutBuffer<Config> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    impl<Config: OutBufferConfig> OutBuffer<Config> {
        pub(crate) fn new(stream: Config::Stream) -> Self {
            let mut s = Self {
                active_chunks: Default::default(),
                inactive_chunks: Default::default(),
                fds: VecDeque::with_capacity(MAX_FDS_OUT),
                stream,
            };
            // there must always be at least one chunk:
            s.active_chunks.push_back(Default::default());
            s
        }

        fn start_next_chunk(&mut self) {
            if !self.inactive_chunks.is_empty() {
                let mut rest = self.inactive_chunks.split_off(1);
                debug_assert_eq!(self.inactive_chunks.len(), 1);
                self.active_chunks.append(&mut self.inactive_chunks);
                self.inactive_chunks = rest;
            } else {
                self.active_chunks.push_back(Default::default());
            }
        }

        fn is_empty(&self) -> bool {
            self.active_chunks.front().expect(MUST_HAVE1).len() == 0
        }

        fn end_mut(&mut self) -> &mut Chunk {
            self.active_chunks.back_mut().expect(MUST_HAVE1)
        }

        fn too_many_fds(&self) -> bool {
            self.fds.len() > self.active_chunks.len() * MAX_FDS_OUT
        }

        // for each msg, push fds first, then push the msg.  This is because we want any fds to be
        // associated with the earliest possible data chunk to help prevent fd starvation of the
        // receiver.
        pub(crate) fn push_fd(&mut self, fd: OwnedFd) -> IoResult<()> {
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
            Ok(())
        }

        pub(crate) fn push(&mut self, msg: &[u8]) -> IoResult<usize> {
            let end_chunk = self.end_mut();
            let split_point = MAX_BYTES_OUT - end_chunk.len();
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

            // This flush is not forced because somewhere up the call chain there is someone
            // responsible for forcing a flush after all pending message traffic has been processed.
            // This flush is only for excess (after the first chunk), and is not necessary.  It's
            // benefit is that it may raise the throughput by allowing more parallelism between
            // sender and receiver.  It may also limit the number of chunks needed in this
            // OutBuffer.
            self.flush(false)
        }

        pub(crate) fn flush(&mut self, force: bool) -> IoResult<usize> {
            let mut total = 0;
            // self.active_chunks.len() > 1 means that there are chunks waiting to get flushed that
            // were refused prevoiusly because the kernel socket buffer did not have room - those
            // should always get flushed if there is now room.  The final chunk should only get
            // flushed after all pending message traffic has been processed - meaning just before
            // starting what might be a traffic lull.  Or when we get signalled that there's now
            // room in the kernel socket buffer for messages we tried to flush previously.
            while self.active_chunks.len() > 1 || force {
                let flushed = self.flush_first_chunk()?;
                if flushed == 0 {
                    break;
                }
                total += flushed;
            }
            Ok(total)
        }

        fn flush_first_chunk(&mut self) -> IoResult<usize> {
            let first_chunk = self.active_chunks.front_mut().expect(MUST_HAVE1);
            if first_chunk.is_empty() {
                return Ok(0);
            }
            let nfds = std::cmp::min(self.fds.len(), MAX_FDS_OUT);
            let flushed = if nfds > 0 {
                let mut bfds = ArrayVec::<_, MAX_FDS_OUT>::new(); // or Vec::with_capacity(nfds)
                bfds.extend(self.fds.iter().take(nfds).map(OwnedFd::as_fd));
                self.stream.send(first_chunk, &bfds)?
            } else {
                self.stream.send(first_chunk, &[])?
            };
            // if flushed == 0, then nothing, not even the fds were flushed
            if flushed > 0 {
                // If the flush happened, then we expect the whole chunk was taken:
                debug_assert_eq!(flushed, first_chunk.len());
                // remove flushed fds, which will close the corresponding files:
                self.fds.drain(..nfds);
                // remove flushed msg data:
                first_chunk.clear();
                if self.active_chunks.len() > 1 {
                    // there are other active chunks, so first_chunk needs to be popped and possibly
                    // moved to inactive_chunks
                    let mut rest_active_chunks = self.active_chunks.split_off(1);
                    debug_assert_eq!(self.active_chunks.len(), 1);
                    // if we are below the excess threshold, keep it, else let it drop
                    if self.inactive_chunks.len() < Config::ALLOWED_EXCESS_CHUNKS {
                        self.inactive_chunks.append(&mut self.active_chunks);
                    }
                    self.active_chunks = rest_active_chunks;
                    debug_assert!(!self.active_chunks.is_empty());
                }
            }
            Ok(flushed)
        }
    }

    #[derive(Debug)]
    pub(crate) struct WaydaptOutBufferConfig;

    impl OutBufferConfig for WaydaptOutBufferConfig {
        const ALLOWED_EXCESS_CHUNKS: usize = 32;
        type Stream = OutUnixStream;
    }
}

mod handler {
    use super::*;
    use event_loop::*;
    use input_buffer::*;
    use output_buffer::*;

    type WInBuf = InBuffer<WaydaptInBufferConfig>;
    type WOutBuf = OutBuffer<WaydaptOutBufferConfig>;

    type WInStream = <WaydaptInBufferConfig as InBufferConfig>::Stream;
    type WOutStream = <WaydaptOutBufferConfig as OutBufferConfig>::Stream;

    // The implementer of InputHandler and the caller of Session::new have to agree on the order of
    // the outbufs/streams: which is for sending events (to the client) and which is for sending
    // requests (to the server).  The code here doesn't care.
    // TBD: maybe pass in a trait instead of WOutBuf to limit the InputHandler?
    pub(crate) trait InputHandler {
        fn handle<P>(
            &mut self, msg: &[u8], fds: &mut P, outbufs: &mut [WOutBuf; 2],
        ) -> IoResult<()>
        where
            P: PopFds;
    }

    #[derive(Debug)]
    pub(crate) struct Session<M: InputHandler> {
        inbufs: [WInBuf; 2],
        outbufs: [WOutBuf; 2],
        handler: M,
    }

    impl<M: InputHandler> Session<M> {
        pub(crate) fn new(handler: M, ins: [WInStream; 2], outs: [WOutStream; 2]) -> Self {
            Self { handler, inbufs: ins.map(InBuffer::new), outbufs: outs.map(OutBuffer::new) }
        }

        pub(crate) fn run(&mut self) -> IoResult<()> {
            EventLoop::event_loop(self)
        }
    }

    impl<M: InputHandler> EventLoop for Session<M> {
        fn in_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
            self.inbufs.iter().map(|b| b.as_fd())
        }
        fn out_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
            self.outbufs.iter().map(|b| b.as_fd())
        }

        fn handle_input(&mut self, index: usize) -> IoResult<()> {
            // By trying receive repeatedly until there's nothing left, we can use edge triggered IN
            // events, which may give higher performance:
            // https://thelinuxcode.com/epoll-7-c-function/
            // But we allow level triggered, and also allow limiting receives
            const MAX_RECEIVES_PER_INPUT: u32 = 1;
            let mut receive_count = 0;
            let inbuf = &mut self.inbufs[index];
            while inbuf.receive()? > 0 {
                receive_count += 1;
                // We expect that at least one whole msg is received.  But we can handle things if
                // that's not the case.
                let mut msg_count = 0u32;
                while let Some((msg, fds)) = inbuf.try_pop() {
                    msg_count += 1;
                    self.handler.handle(msg, fds, &mut self.outbufs)?;
                }
                debug_assert!(msg_count > 0);
                // Only stop if we have reached the input limit AND the input wasn't edge triggered.
                // If the input was edge triggered, we must receive everything until told there is
                // no more (by receive returning 0).
                if receive_count >= MAX_RECEIVES_PER_INPUT && !Self::EDGE_TRIGGER_INPUT {
                    break;
                }
            }
            // If MAX_RECEIVES_PER_INPUT > 1, then it might be possible that we were told to handle
            // input that we already handled in the previous round.  There may be a data race
            // between us and the kernel that could cause this.  But if we didn't do any receives
            // this round, it's not a problem.
            debug_assert!(receive_count > 0 || MAX_RECEIVES_PER_INPUT > 1);
            if receive_count > 0 {
                // If we received anything, we probably generated output.  Usually that will happen
                // on the opposite outbuf (1-index), but in the future when waydapt can originate
                // its own messages, it could be either or both.
                for b in self.outbufs.iter_mut() {
                    // Force flush because we don't know when we will be back here, so waiting to
                    // flush any part might starve the receiver.
                    b.flush(/*force:*/ true)?;
                }
            }
            Ok(())
        }

        fn handle_output(&mut self, index: usize) -> IoResult<()> {
            // If we don't force the flush to include the last partial chunk, what will cause that
            // last partial chunk to get sent?  Maybe nothing.  So we have to force it here.
            let amount = self.outbufs[index].flush(/*force:*/ true)?;
            // should we check the amount flushed?
            debug_assert!(amount > 0);
            Ok(())
        }
    }
}
