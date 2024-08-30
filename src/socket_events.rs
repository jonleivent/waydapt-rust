use crate::crate_traits::EventHandler;
use crate::for_handlers::SessionInitHandler;
use crate::listener::SocketListener;
use crate::postparse::ActiveInterfaces;
use crate::session::client_session;
use crate::setup::SharedOptions;
use nix::sys::signal;
use nix::sys::signalfd::{SfdFlags, SignalFd};
use rustix::event::epoll::EventFlags;
use signal::{SigSet, Signal};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

pub(crate) struct SocketEventHandler {
    listener: SocketListener,
    signalfd: SignalFd,
    options: &'static SharedOptions,
    interfaces: &'static ActiveInterfaces,
    handlers: &'static VecDeque<SessionInitHandler>,
}

impl SocketEventHandler {
    pub(crate) fn new(
        listener: SocketListener, options: &'static SharedOptions,
        interfaces: &'static ActiveInterfaces, handlers: &'static VecDeque<SessionInitHandler>,
    ) -> Self {
        // Mask and handle these signals:
        let mut mask = SigSet::empty();
        mask.add(signal::SIGTERM);
        mask.add(signal::SIGHUP);
        mask.add(signal::SIGINT);
        mask.add(signal::SIGUSR1); // for terminate_main_thread
        mask.thread_block().unwrap();
        let signalfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK).unwrap();

        SocketEventHandler { listener, signalfd, options, interfaces, handlers }
    }

    fn spawn_session(&self, client_stream: UnixStream) {
        let options = self.options;
        let interfaces = self.interfaces;
        let handlers = self.handlers;
        let session = move || client_session(options, interfaces, handlers, client_stream);
        std::thread::spawn(session);
    }

    fn handle_signal_input(&mut self) -> IoResult<Option<()>> {
        #[allow(clippy::cast_possible_wrap)]
        let sig = match self.signalfd.read_signal() {
            Ok(Some(ref sig)) => sig.ssi_signo as i32,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if sig == signal::SIGUSR1 as i32 {
            Ok(Some(())) // use SIGUSR1 for normal termination
        } else {
            let signal = Signal::try_from(sig).unwrap();
            let err = Error::new(ErrorKind::Interrupted, signal.as_ref());
            Err(err)
        }
    }

    fn handle_listener_input(&mut self) -> IoResult<Option<()>> {
        let client_stream = match self.listener.accept() {
            Ok((client_stream, _)) => client_stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        self.spawn_session(client_stream);
        Ok(None)
    }
}

impl EventHandler for SocketEventHandler {
    type InputResult = ();

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)> {
        let flags = EventFlags::IN; // level triggered
        [(self.listener.as_fd(), flags), (self.signalfd.as_fd(), flags)].into_iter()
    }

    fn handle_input(&mut self, fd_index: usize) -> IoResult<Option<()>> {
        match fd_index {
            0 => self.handle_listener_input(),
            1 => self.handle_signal_input(),
            _ => unreachable!(),
        }
    }

    fn handle_output(&mut self, _fd_index: usize) -> IoResult<()> {
        unreachable!("We didn't ask for no output!")
    }
}
