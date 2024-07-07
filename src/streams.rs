#![warn(clippy::pedantic)]
//#![forbid(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

use crate::basics::{init_array, MAX_FDS_OUT};
use crate::crate_traits::{InStream, OutStream};
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_OUT));

#[derive(Debug)]
pub(crate) struct IOStream {
    stream: UnixStream,
    cmsg_space: [u8; CMSG_SPACE],
}

impl IOStream {
    pub(crate) fn new(stream: UnixStream) -> Self {
        Self { stream, cmsg_space: init_array(0) }
    }
}

impl AsFd for IOStream {
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

impl InStream for IOStream {
    fn receive<T>(&mut self, buf: &mut [T], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize> {
        use rustix::io::{retry_on_intr, Errno, IoSliceMut};
        use rustix::net::{recvmsg, RecvFlags};
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage};

        let (start_pad, buf, end_pad) = unsafe { buf.align_to_mut::<u8>() };
        debug_assert!(start_pad.is_empty() && end_pad.is_empty());

        let flags = RecvFlags::DONTWAIT;
        #[cfg(not(target_os = "macos"))]
        let flags = flags | RecvFlags::CMSG_CLOEXEC;

        let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut self.cmsg_space);
        let mut iov = [IoSliceMut::new(buf)];
        let recv = || recvmsg(&self.stream, &mut iov, &mut cmsg_buffer, flags);
        let bytes = match retry_on_intr(recv) {
            Ok(b) => b.bytes,
            Err(e) if e == Errno::WOULDBLOCK => 0,
            Err(e) if e == Errno::AGAIN => 0,
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

impl OutStream for IOStream {
    fn send<T>(&mut self, data: &[T], fds: &[BorrowedFd<'_>]) -> IoResult<usize> {
        use rustix::io::{retry_on_intr, Errno, IoSlice};
        use rustix::net::{send, sendmsg, SendFlags};
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};

        let (start_pad, data, end_pad) = unsafe { data.align_to::<u8>() };
        debug_assert!(start_pad.is_empty() && end_pad.is_empty());

        let flags = SendFlags::DONTWAIT;
        #[cfg(not(target_os = "macos"))]
        let flags = flags | SendFlags::NOSIGNAL;

        let outfd = self.stream.as_fd();
        let result = if fds.is_empty() {
            retry_on_intr(|| send(outfd, data, flags))
        } else {
            debug_assert!(fds.len() <= MAX_FDS_OUT);
            let iov = [IoSlice::new(data)];
            let mut cmsg_buffer = SendAncillaryBuffer::new(&mut self.cmsg_space);
            let pushed = cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds));
            debug_assert!(pushed);
            retry_on_intr(|| sendmsg(outfd, &iov, &mut cmsg_buffer, flags))
        };
        match result {
            Ok(b) => Ok(b),
            Err(e) if e == Errno::WOULDBLOCK => Ok(0),
            Err(e) if e == Errno::AGAIN => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}
