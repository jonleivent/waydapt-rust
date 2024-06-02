#![allow(unused)]
#![forbid(unsafe_code)]

use arrayvec::ArrayVec;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::io::{IoSlice, IoSliceMut};
use std::marker::PhantomData;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use rustix::net::{
    recvmsg, send, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, Shutdown,
};

const MAX_FDS_OUT: usize = 28;
const MAX_BYTES_OUT: usize = 4096;
const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_OUT));

// The overall usage pattern should be:
// 0. epoll for in socket ready
// 1. receive
// 2. pop msg, parse it, pop its fds
// 3. handle msg
// 4. push msg's fds and push msg
// 5. if have more in msgs in inbuffer, goto 2, else force flush
// 6. epoll in socket, and out socket if needs_flushing
// 7. if out socket ready, force flush, goto 6
// 8. if in socket ready, goto 1

// suppose we try to do a force flush in 5, but the socket is full.  We then epoll in 6, and the
// socket becomes unfull.  In 7, do we force flush or just flush?  We have to force flush, else the
// last msg might not get sent.  I think this means that 7 always does a force flush.  Which means
// the caller never does a unforced flush.

mod epollable {
    use epoll::{CreateFlags, Event, EventData, EventFlags, EventVec};
    use rustix::event::epoll;
    use std::io::Result as IoResult;
    use std::os::fd::{BorrowedFd, OwnedFd};

    pub(crate) trait Epollable {
        fn in_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>>;
        fn out_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>>;
        fn handle_input(&mut self, fd_index: usize) -> IoResult<()>;
        fn handle_output(&mut self, fd_index: usize) -> IoResult<()>;

        fn in_flags() -> EventFlags {
            EventFlags::IN | EventFlags::ET // Edge Triggered
        }

        fn must_receive_all() -> bool {
            Self::in_flags().contains(EventFlags::ET)
        }

        fn out_flags() -> EventFlags {
            // The output has to be edge triggered, as we don't want to get an event everytime we
            // wait while there is room in the kernel socket buffer.  Maybe we force this?
            EventFlags::OUT | EventFlags::ET // Edge Triggered
        }

        fn handle_error(
            &mut self, fd_index: usize, in_len: usize, flags: EventFlags,
        ) -> IoResult<()> {
            use std::io::{Error, ErrorKind};
            Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("{:?}", flags.iter_names().collect::<Vec<_>>()),
            ))
        }

        fn handle_all(
            &mut self, fd_index: usize, in_len: usize, flags: EventFlags,
        ) -> IoResult<()> {
            if flags.intersects(Self::in_flags() | Self::out_flags()) {
                if fd_index < in_len {
                    self.handle_input(fd_index)
                } else {
                    self.handle_output(fd_index - in_len)
                }
            } else {
                self.handle_error(fd_index, in_len, flags)
            }
        }

        fn event_loop(&mut self) -> IoResult<()> {
            let epoll_fd = epoll::create(CreateFlags::CLOEXEC)?;
            let in_fds = self.in_fds();
            let mut i = 0;
            for fd in in_fds {
                let data = EventData::new_u64(i);
                epoll::add(&epoll_fd, fd, data, Self::in_flags());
                i += 1;
            }
            let in_len = i as usize;
            let out_fds = self.out_fds();
            for fd in out_fds {
                let data = EventData::new_u64(i);
                // output has to be edge triggered (ET), because otherwise we would get an event
                // whenever there is room in the kernel socket buffer
                epoll::add(&epoll_fd, fd, data, Self::out_flags() | EventFlags::ET);
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
                    self.handle_all(i, in_len, flags)?;
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
    use std::io::empty;

    use rustix::net::RecvMsgReturn;

    use super::*;

    pub(crate) trait InStream {
        fn receive(&mut self, buf: &mut [u8], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize>;
    }

    pub(crate) struct InUnixStream {
        stream: UnixStream,
        cmsg_space: [u8; CMSG_SPACE],
    }

    impl InUnixStream {
        pub(crate) fn new(stream: UnixStream) -> Self {
            Self { stream, cmsg_space: [0; CMSG_SPACE] }
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

    use rustix::io::Errno;
    fn retry_or_convert<F>(mut f: F) -> IoResult<usize>
    where
        F: FnMut() -> Result<RecvMsgReturn, rustix::io::Errno>,
    {
        match rustix::io::retry_on_intr(f) {
            Ok(b) => Ok(b.bytes),
            Err(Errno::WOULDBLOCK) | Err(Errno::AGAIN) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    impl InStream for InUnixStream {
        fn receive(&mut self, buf: &mut [u8], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize> {
            let flags = RecvFlags::DONTWAIT;
            #[cfg(not(target_os = "macos"))]
            let flags = flags | RecvFlags::CMSG_CLOEXEC;

            let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut self.cmsg_space);
            let mut iov = [IoSliceMut::new(buf)];
            let recv = || recvmsg(&self.stream, &mut iov[..], &mut cmsg_buffer, flags);
            let bytes = retry_or_convert(recv)?;
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

    pub(crate) struct InBuffer<Config: InBufferConfig> {
        // there is probably no benefit to allocating storage in-line (using ArrayVec) instead of in
        // a Vec because if it is in-line, then this shouldn't be on the stack, otherwise it can be.
        storage: Vec<u8>,
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
                storage: vec![0; MAX_BYTES_OUT * 2], // *2 on purpose!
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
            assert!(self.back <= MAX_BYTES_OUT);
            let buf = &mut self.storage[self.back..(MAX_BYTES_OUT + self.back)];
            let amount_read = self.stream.receive(buf, &mut self.fds)?;
            self.back += amount_read;
            Ok(amount_read)
        }

        fn compact(&mut self) {
            let len = self.back - self.front;
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

    pub(crate) trait OutStream {
        fn send(&mut self, data: &[u8], fds: &[BorrowedFd<'_>]) -> IoResult<usize>;
    }

    pub(crate) struct OutUnixStream {
        stream: UnixStream,
        cmsg_space: [u8; CMSG_SPACE],
    }

    impl OutUnixStream {
        pub(crate) fn new(stream: UnixStream) -> Self {
            Self { stream, cmsg_space: [0; CMSG_SPACE] }
        }
    }

    impl AsFd for OutUnixStream {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    use rustix::io::Errno;
    fn retry_or_convert<F>(mut f: F) -> IoResult<usize>
    where
        F: FnMut() -> rustix::io::Result<usize>,
    {
        match rustix::io::retry_on_intr(f) {
            Ok(b) => Ok(b),
            Err(Errno::WOULDBLOCK) | Err(Errno::AGAIN) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    impl OutStream for OutUnixStream {
        fn send(&mut self, data: &[u8], fds: &[BorrowedFd<'_>]) -> IoResult<usize> {
            let flags = SendFlags::DONTWAIT;
            #[cfg(not(target_os = "macos"))]
            let flags = flags | SendFlags::NOSIGNAL;

            let outfd = self.stream.as_fd();
            if !fds.is_empty() {
                let iov = [IoSlice::new(data)];
                let mut cmsg_buffer = SendAncillaryBuffer::new(&mut self.cmsg_space);
                cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds));
                retry_or_convert(|| sendmsg(outfd, &iov, &mut cmsg_buffer, flags))
            } else {
                retry_or_convert(|| send(outfd, data, flags))
            }
        }
    }

    pub(crate) trait OutBufferConfig {
        const ALLOWED_EXCESS_CHUNKS: usize;
        type Stream: OutStream + AsFd;
    }

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
        chunks: VecDeque<Vec<u8>>,
        end: usize, // index into chunks
        fds: VecDeque<OwnedFd>,
        stream: Config::Stream,
    }

    impl<Config: OutBufferConfig> AsFd for OutBuffer<Config> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.stream.as_fd()
        }
    }

    impl<Config: OutBufferConfig> OutBuffer<Config> {
        fn extend(&mut self) {
            self.chunks.push_back(Vec::with_capacity(MAX_BYTES_OUT));
        }

        pub(crate) fn new(stream: Config::Stream) -> Self {
            let mut s = Self {
                chunks: VecDeque::new(),
                end: 0,
                fds: VecDeque::with_capacity(MAX_FDS_OUT),
                stream,
            };
            // there must always be at least one chunk:
            s.extend();
            s
        }

        fn start_next_chunk(&mut self) {
            self.end += 1;
            if self.end == self.chunks.len() {
                self.extend()
            }
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.chunks[0].len() == 0
        }

        // for each msg, push fds first, then push the msg.  This is because we want any fds to be
        // associated with the earliest possible data chunk to help prevent fd starvation of the
        // receiver.
        pub(crate) fn push_fd(&mut self, fd: OwnedFd) {
            self.fds.push_back(fd);
            // this condition prevents the possibility of having fds in the OutBuffer but no msg
            // data, which can cause starvation of the receiver.  This can happen whenever there are
            // more fds than can be flushed with the existing data, due to the MAX_FDS_OUT limit.
            // But it only works if this is called before push (which is the expected workflow).
            if self.fds.len() > self.end * MAX_FDS_OUT {
                // We have to start the next chunk because we can't force a flush and hope it works.
                // It may not work because the kernel socket buffer might be too full.
                self.start_next_chunk();
                assert!(self.fds.len() <= self.end * MAX_FDS_OUT);
            }
        }

        pub(crate) fn push(&mut self, msg: &[u8]) -> IoResult<usize> {
            let end_chunk = self.end_mut();
            let split_point = MAX_BYTES_OUT - end_chunk.len();
            if split_point >= msg.len() {
                // all of msg fits in end chunk
                end_chunk.extend_from_slice(msg);
            } else {
                if split_point > 0 {
                    // some of msg fits in end chunk
                    end_chunk.extend_from_slice(&msg[..split_point]);
                }
                self.start_next_chunk();
                // write rest of msg
                let end_chunk = self.end_mut();
                assert!(end_chunk.is_empty());
                end_chunk.extend_from_slice(&msg[split_point..]);
            }

            // This flush is not forced because somewhere up the call chain there is someone
            // responsible for forcing a flush after all pending msg traffic has been processed.
            // This flush is only for excess (after the first chunk), and is not necessary.  It's
            // benefit is that it may raise the throughput by allowing more parallelism between
            // sender and receiver.  It may also limit the number of chunks needed in this
            // OutBuffer.
            self.flush(false)
        }

        pub(crate) fn needs_flushing(&self) -> bool {
            // true if there's a backlog
            self.end > 0
        }

        pub(crate) fn flush(&mut self, force: bool) -> IoResult<usize> {
            let mut total = 0;
            while force || self.needs_flushing() {
                let flushed = self.flush_first()?;
                if flushed == 0 {
                    break;
                }
                total += flushed;
            }
            Ok(total)
        }

        fn end_mut(&mut self) -> &mut Vec<u8> {
            &mut self.chunks[self.end]
        }

        fn flush_first(&mut self) -> IoResult<usize> {
            let first_chunk = &mut self.chunks[0];
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

            if (flushed > 0) {
                assert_eq!(flushed, first_chunk.len());
                // remove flushed fds:
                self.fds.drain(..nfds);
                // remove flushed msg data:
                first_chunk.clear();
                if self.end > 0 {
                    // there are other active chunks
                    if self.chunks.len() - self.end > Config::ALLOWED_EXCESS_CHUNKS {
                        self.chunks.pop_front();
                    } else {
                        self.chunks.rotate_left(1);
                    }
                    self.end -= 1;
                }
            }
            Ok(flushed)
        }
    }

    pub(crate) struct WaydaptOutBufferConfig;

    impl OutBufferConfig for WaydaptOutBufferConfig {
        const ALLOWED_EXCESS_CHUNKS: usize = 32;
        type Stream = OutUnixStream;
    }
}

use input_buffer::*;
use output_buffer::*;

type WInBuf = InBuffer<WaydaptInBufferConfig>;
type WOutBuf = OutBuffer<WaydaptOutBufferConfig>;

type WInStream = <WaydaptInBufferConfig as InBufferConfig>::Stream;
type WOutStream = <WaydaptOutBufferConfig as OutBufferConfig>::Stream;

pub(crate) trait MessageHandler {
    fn handle<P>(&mut self, msg: &[u8], fds: &mut P, outbufs: &mut [WOutBuf; 2]) -> IoResult<()>
    where
        P: PopFds;
}

pub(crate) struct Session<M: MessageHandler> {
    inbufs: [WInBuf; 2],
    outbufs: [WOutBuf; 2],
    handler: M,
}

impl<M: MessageHandler> Session<M> {
    pub fn new(
        handler: M, upin: WInStream, downin: WInStream, upout: WOutStream, downout: WOutStream,
    ) -> Self {
        Self {
            inbufs: [
                InBuffer::<WaydaptInBufferConfig>::new(upin),
                InBuffer::<WaydaptInBufferConfig>::new(downin),
            ],
            outbufs: [
                OutBuffer::<WaydaptOutBufferConfig>::new(upout),
                OutBuffer::<WaydaptOutBufferConfig>::new(downout),
            ],
            handler,
        }
    }
}

impl<M: MessageHandler> epollable::Epollable for Session<M> {
    fn in_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
        self.inbufs.iter().map(|b| b.as_fd())
    }
    fn out_fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
        self.outbufs.iter().map(|b| b.as_fd())
    }

    fn handle_input(&mut self, index: usize) -> IoResult<()> {
        // It might be beneficial to lump together multiple calls to InBuffer.receive between
        // epoll::waits.  Doing so would potentially speed up the throughput in high traffic
        // situations.  Flushes will be done in between automatically if needed.  The downside is
        // that we potentially starve out the other side - although we don't do anything to provide
        // for fairness even if we did only one receive at a time.  We could - after an input event
        // is handled on one stream, we could check the other stream for input before epolling both
        // again.  Another downside is that we may still get the extra IN events with nothing to
        // receive.  Does recvmsg with a DONTWAIT flag return a WOULDBLOCK error if there's nothing
        // to receive?  That would be a problem as well.  What to do about that?  Changed receive so
        // that it returns 0 in those cases.

        // We could implement fairness by ignoring the index and testing both inbufs every time.
        // Then the event loop should only call this once.  But is fairness an issue?  It's true
        // that Wayland is asynchronous, but there's logic higher up that requires synchrony, and
        // will pause one side vs. the other.  That's after all the reason for sync msgs in the
        // protocol.

        // If we handle all available input with no limit, then we can use edge triggered IN events,
        // which may give higher performance: https://thelinuxcode.com/epoll-7-c-function/
        const MAX_RECEIVES_PER_INPUT: u32 = 1;

        let mut receive_count = 0;
        while self.inbufs[index].receive()? > 0 {
            receive_count += 1;
            // We expect that amount > 0 AND that at least one whole msg is received.  But we can handle
            // things if that's not the case.
            let mut msg_count = 0u32;
            while let Some((msg, fds)) = self.inbufs[index].try_pop() {
                msg_count += 1;
                self.handler.handle(msg, fds, &mut self.outbufs)?;
            }
            assert!(msg_count > 0);
            // Only stop if we have reached the input limit AND the input wasn't edge triggered.  If
            // the input was edge triggered, we are obligated to receive everything until told there
            // is no more (by receive returning 0).
            if receive_count >= MAX_RECEIVES_PER_INPUT && !Self::must_receive_all() {
                break;
            }
        }
        assert!(receive_count > 0 || MAX_RECEIVES_PER_INPUT > 1);
        if receive_count > 0 {
            // If we received anything, we probably generated output.  Usually that will happen on
            // the opposite outbuf (1-index), but in the future when waydapt can originate its own
            // messages, it could be either or both.
            for b in self.outbufs.iter_mut() {
                // A force flush means we don't know when we will be back here, so we can't wait to
                // flush because that might starve the receiver.
                b.flush(/*force:*/ true)?;
            }
        }
        Ok(())
    }

    fn handle_output(&mut self, index: usize) -> IoResult<()> {
        // If we don't force the flush to include the last partial chunk, what will cause that last
        // partial chunk to get sent?  Maybe nothing.
        let amount = self.outbufs[index].flush(/*force:*/ true)?;
        // should we check the amount flushed?
        assert!(amount > 0);
        Ok(())
    }
}
