#![allow(clippy::inline_always)]

use std::io::Result as IoResult;
use std::os::fd::OwnedFd;

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
