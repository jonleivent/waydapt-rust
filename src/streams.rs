#![warn(clippy::pedantic)]
//#![forbid(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

use crate::basics::{to_u8_slice, to_u8_slice_mut, uninit_array, MAX_FDS_OUT};
use crate::crate_traits::{AllBitValuesSafe, InStream, OutStream};
use rustix::io::{retry_on_intr, Errno, IoSlice, IoSliceMut};
use rustix::net::{recvmsg, RecvFlags};
use rustix::net::{send, sendmsg, SendFlags};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
use std::io::Result as IoResult;
use std::mem::size_of;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_OUT));

#[derive(Debug)]
pub(crate) struct IOStream {
    stream: UnixStream,
}

impl IOStream {
    pub(crate) fn new(stream: UnixStream) -> Self {
        Self { stream }
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
    fn receive<T>(&self, buf: &mut [T], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize>
    where
        T: AllBitValuesSafe,
    {
        let byte_buf = to_u8_slice_mut(buf);

        let flags = RecvFlags::DONTWAIT;
        #[cfg(not(target_os = "macos"))]
        let flags = flags | RecvFlags::CMSG_CLOEXEC;

        let mut cmsg_space: [u8; CMSG_SPACE] = uninit_array();
        let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut cmsg_space);
        let mut iov = [IoSliceMut::new(byte_buf)];
        let recv = || recvmsg(self, &mut iov, &mut cmsg_buffer, flags);
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
        let per = size_of::<T>();
        assert_eq!(bytes % per, 0);
        Ok(bytes / per)
    }
}

impl OutStream for IOStream {
    fn send<T>(&self, data: &[T], fds: &[BorrowedFd<'_>]) -> IoResult<usize>
    where
        T: AllBitValuesSafe,
    {
        let byte_data = to_u8_slice(data);

        let flags = SendFlags::DONTWAIT;
        #[cfg(not(target_os = "macos"))]
        let flags = flags | SendFlags::NOSIGNAL;

        let result = if fds.is_empty() {
            retry_on_intr(|| send(self, byte_data, flags))
        } else {
            debug_assert!(fds.len() <= MAX_FDS_OUT);
            let iov = [IoSlice::new(byte_data)];
            let mut cmsg_space: [u8; CMSG_SPACE] = uninit_array();
            let mut cmsg_buffer = SendAncillaryBuffer::new(&mut cmsg_space);
            let pushed = cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds));
            debug_assert!(pushed);
            retry_on_intr(|| sendmsg(self, &iov, &mut cmsg_buffer, flags))
        };
        match result {
            Ok(bytes) => {
                assert_eq!(bytes, byte_data.len());
                let per = size_of::<T>();
                Ok(bytes / per)
            }
            Err(e) if e == Errno::WOULDBLOCK => Ok(0),
            Err(e) if e == Errno::AGAIN => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}
