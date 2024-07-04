use rustix::event::epoll::EventFlags;
use std::io::Result as IoResult;
use std::os::fd::{BorrowedFd, OwnedFd};

pub(crate) trait InStream {
    fn receive(&mut self, buf: &mut [u8], fds: &mut impl Extend<OwnedFd>) -> IoResult<usize>;
}

pub(crate) trait OutStream {
    fn send(&mut self, data: &[u8], fds: &[BorrowedFd<'_>]) -> IoResult<usize>;
}

pub(crate) trait FdInput {
    fn try_take_fd(&mut self) -> Option<OwnedFd>;
}

pub(crate) trait MessageSender {
    fn send(
        &mut self, fds: impl Iterator<Item = OwnedFd>, msgfun: impl FnOnce(&mut [u8]) -> usize,
    ) -> IoResult<usize>;
}

pub(crate) trait Messenger {
    type FI: FdInput;
    type MS: MessageSender;

    fn handle(
        &mut self, from: usize, in_msg: &[u8], in_fds: &mut Self::FI, outs: &mut [Self::MS; 2],
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
