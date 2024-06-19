#![forbid(unsafe_code)]

use crate::buffers::OutBuffer;
use crate::streams::OutStream;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

pub(crate) trait OutChannel {
    fn push_fd(&mut self, fd: OwnedFd) -> IoResult<()>;

    fn push(&mut self, msg: &[u8]) -> IoResult<usize>;
}

pub(crate) trait PopFds {
    fn pop(&mut self) -> Option<OwnedFd>;
}

pub(crate) trait InputHandler {
    fn handle<P, C>(&mut self, in_msg: &[u8], in_fds: &mut P, output: &mut [C; 2]) -> IoResult<()>
    where
        P: PopFds,
        C: OutChannel;
}

////////////////////////////////////////////////////////////////////////////////

impl PopFds for VecDeque<OwnedFd> {
    fn pop(&mut self) -> Option<OwnedFd> {
        self.pop_front()
    }
}

impl<S: OutStream> OutChannel for OutBuffer<S> {
    fn push_fd(&mut self, fd: OwnedFd) -> IoResult<()> {
        self.push_fd(fd)
    }

    fn push(&mut self, msg: &[u8]) -> IoResult<usize> {
        self.push(msg)
    }
}
