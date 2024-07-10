use rustix::event::epoll::EventFlags;
use std::io::Result as IoResult;
use std::os::fd::{BorrowedFd, OwnedFd};

use crate::buffers::ExtendChunk;
use crate::header::MessageHeader;
use crate::map::WL_SERVER_ID_START;

pub(crate) trait Peer {
    const IS_SERVER: bool;

    fn normalize_id(id: u32) -> usize;
}

pub(crate) struct ClientPeer;

impl Peer for ClientPeer {
    const IS_SERVER: bool = false;

    fn normalize_id(id: u32) -> usize {
        assert!(id < WL_SERVER_ID_START, "Wrong side id");
        id as usize
    }
}

pub(crate) struct ServerPeer;

impl Peer for ServerPeer {
    const IS_SERVER: bool = true;

    fn normalize_id(id: u32) -> usize {
        assert!(id >= WL_SERVER_ID_START, "Wrong side id");
        (id - WL_SERVER_ID_START) as usize
    }
}

pub(crate) trait InStream {
    fn receive<T>(&self, buf: &mut [T], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize>
    where
        T: AllBitValuesSafe;
}

pub(crate) trait OutStream {
    fn send<T>(&self, data: &[T], fds: &[BorrowedFd<'_>]) -> IoResult<usize>
    where
        T: AllBitValuesSafe;
}

pub(crate) trait FdInput {
    fn try_take_fd(&mut self) -> Option<OwnedFd>;
}

pub(crate) trait MessageSender {
    fn send(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>,
        msgfun: impl FnOnce(ExtendChunk) -> MessageHeader,
    ) -> IoResult<usize>;

    fn send_raw(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>, raw_msg: &[u32],
    ) -> IoResult<usize>;
}

pub(crate) trait Messenger {
    type FI: FdInput;
    type MO: MessageSender;

    fn handle(
        &mut self, from: usize, in_msg: &[u32], in_fds: &mut Self::FI, out: &mut Self::MO,
    ) -> IoResult<()>;
}

pub(crate) trait EventHandler {
    fn fds_to_monitor(&self) -> impl Iterator<Item = BorrowedFd<'_>>;

    fn handle_input(&mut self, fd_index: usize) -> IoResult<()>;

    fn handle_output(&mut self, fd_index: usize) -> IoResult<()>;

    fn handle_error(&mut self, _fd_index: usize, flags: EventFlags) -> IoResult<()> {
        use std::io::{Error, ErrorKind};
        // TBD - maybe improve error messages for some sets of flags
        Err(Error::new(
            ErrorKind::ConnectionAborted,
            format!("event flags: {:?}", flags.iter_names().collect::<Vec<_>>()),
        ))
    }
}

// ImplAsBytes should only be implemented for types that are implemented as bytes without
// translation, and without any possible unsafe bit values - such as the primitive numeric types
pub(crate) unsafe trait AllBitValuesSafe {}
