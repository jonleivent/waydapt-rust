#![forbid(unsafe_code)]
use crate::basics::{to_u8_slice, to_u8_slice_mut, MAX_FDS_OUT};
use rustix::io::{retry_on_intr, Errno, IoSlice, IoSliceMut};
use rustix::net::{recvmsg, RecvFlags};
use rustix::net::{send, sendmsg, SendFlags};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
use std::io::Result as IoResult;
use std::os::unix::io::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_OUT));

pub(crate) type IOStream = UnixStream;

#[cfg(target_os = "macos")]
fn cloexec_fd(fd: Fd) {
    use rustix::io;
    if let Ok(flags) = io::fcntl_getfd(fd) {
        let _ = io::fcntl_setfd(fd, flags | io::FdFlags::CLOEXEC);
    }
}

pub(crate) fn recv_msg<F>(stream: &IOStream, buf: &mut [u32], fds: &mut F) -> IoResult<usize>
where F: Extend<OwnedFd> {
    let byte_buf = to_u8_slice_mut(buf);

    let flags = RecvFlags::DONTWAIT;
    #[cfg(not(target_os = "macos"))]
    let flags = flags | RecvFlags::CMSG_CLOEXEC;

    let mut cmsg_space = [0u8; CMSG_SPACE]; // must be 0 init: see cmsg(3) CMSG_NXTHDR
    let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut cmsg_space);
    let mut iov = [IoSliceMut::new(byte_buf)];
    let recv = || recvmsg(stream, &mut iov, &mut cmsg_buffer, flags);
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
    assert_eq!(bytes % 4, 0);
    Ok(bytes / 4)
}

pub(crate) fn send_msg(stream: &IOStream, data: &[u32], fds: &[BorrowedFd<'_>]) -> IoResult<usize> {
    let byte_data = to_u8_slice(data);

    let flags = SendFlags::DONTWAIT;
    #[cfg(not(target_os = "macos"))]
    let flags = flags | SendFlags::NOSIGNAL;

    let result = if fds.is_empty() {
        retry_on_intr(|| send(stream, byte_data, flags))
    } else {
        assert!(fds.len() <= MAX_FDS_OUT);
        let iov = [IoSlice::new(byte_data)];
        let mut cmsg_space = [0u8; CMSG_SPACE]; // must be 0 init: see cmsg(3) CMSG_NXTHDR
        let mut cmsg_buffer = SendAncillaryBuffer::new(&mut cmsg_space);
        assert!(cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds)));
        retry_on_intr(|| sendmsg(stream, &iov, &mut cmsg_buffer, flags))
    };
    match result {
        Ok(bytes) => {
            // We are using all-or-nothing sends - but do we need to?  The code in flush_first_chunk
            // assumes it in order to simplify chunk management.  If we didn't send all-or-nothing,
            // then we would have to deal with partial chunk draining at the very least.  I think we
            // have to use all-or-nothing sends because of the MAX_FDS_OUT vs. fd starvation issue.
            assert_eq!(bytes, byte_data.len());
            assert_eq!(bytes % 4, 0);
            Ok(bytes / 4)
        }
        Err(e) if e == Errno::WOULDBLOCK => Ok(0),
        Err(e) if e == Errno::AGAIN => Ok(0),
        Err(e) => Err(e.into()),
    }
}
