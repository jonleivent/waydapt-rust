#![allow(clippy::inline_always)]

use rustix::event::epoll::EventFlags;
use std::io::Result as IoResult;
use std::os::fd::{BorrowedFd, OwnedFd};

use crate::buffers::ExtendChunk;
use crate::header::MessageHeader;

pub(crate) trait Alloc {
    #[allow(clippy::mut_from_ref)]
    fn alloc<T>(&self, it: T) -> &mut T;
}

pub(crate) trait FdInput {
    fn try_take_fd(&mut self) -> Option<OwnedFd>;

    fn drain(&mut self, num: usize) -> impl Iterator<Item = OwnedFd>;
}

pub(crate) trait Messenger {
    fn send(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>,
        msgfun: impl FnOnce(ExtendChunk) -> MessageHeader,
    ) -> IoResult<usize>;

    fn send_raw(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>, raw_msg: &[u32],
    ) -> IoResult<usize>;
}

pub(crate) trait EventHandler {
    type InputResult;

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)>;

    fn handle_input(&mut self, fd_index: usize) -> IoResult<Option<Self::InputResult>>;

    fn handle_output(&mut self, fd_index: usize) -> IoResult<()>;

    fn handle_error(&mut self, _fd_index: usize, flags: EventFlags) -> IoResult<()> {
        use std::io::{Error, ErrorKind};
        if flags == EventFlags::HUP {
            Err(Error::new(ErrorKind::ConnectionAborted, "normal HUP termination"))
        } else {
            Err(Error::new(
                ErrorKind::ConnectionAborted,
                format!("event flags: {:?}", flags.iter_names().collect::<Vec<_>>()),
            ))
        }
    }
}

// ImplAsBytes should only be implemented for types that are implemented as bytes without
// translation, and without any possible unsafe bit values - such as the primitive numeric types
#[allow(clippy::missing_safety_doc)]
#[allow(unsafe_code)]
pub(crate) unsafe trait AllBitValuesSafe {}
