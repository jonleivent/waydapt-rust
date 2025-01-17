use crate::event_loop::{EventHandler, ElResult, EventLoopFlow};
use crate::handlers::SessionHandlers;
use crate::listener::SocketListener;
use crate::postparse::ActiveInterfaces;
use crate::session::client_session;
use crate::setup::SharedOptions;
use nix::sys::signal;
use nix::sys::signalfd::{SfdFlags, SignalFd};
use rustix::event::epoll::EventFlags;
use signal::{SigSet, Signal};
use std::io::{Error, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

pub(crate) struct SocketEventHandler {
    listener: SocketListener,
    signalfd: SignalFd,
    server_stream: Option<UnixStream>,
    options: &'static SharedOptions,
    interfaces: &'static ActiveInterfaces,
    handlers: SessionHandlers,
}

impl SocketEventHandler {
    pub(crate) fn new(
        listener: SocketListener, options: &'static SharedOptions,
        interfaces: &'static ActiveInterfaces, handlers: SessionHandlers,
    ) -> Self {
        // Mask and handle these signals:
        let mut mask = SigSet::empty();
        mask.add(signal::SIGTERM);
        mask.add(signal::SIGHUP);
        mask.add(signal::SIGINT);
        mask.add(signal::SIGUSR1); // for terminate_main_thread
        mask.thread_block().unwrap();
        let signalfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK).unwrap();
        let server_stream = options.watch_server.then(crate::session::get_server_stream);

        SocketEventHandler { listener, signalfd, server_stream, options, interfaces, handlers }
    }

    fn spawn_session(&self, client_stream: UnixStream) {
        let options = self.options;
        let interfaces = self.interfaces;
        let handlers = self.handlers;
        let session = move || client_session(options, interfaces, handlers, client_stream);
        std::thread::spawn(session);
    }

    fn err_for_signo(signo: i32) -> Error {
        if signo == signal::SIGUSR1 as i32 {
            Error::other("Internal signal")
        } else if let Ok(signal) = Signal::try_from(signo) {
            Error::new(ErrorKind::Interrupted, signal.as_ref())
        } else {
            Error::from(ErrorKind::Interrupted)
        }
    }

    fn handle_signal_input(&self) -> ElResult<UnixStream> {
        #[allow(clippy::cast_possible_wrap)]
        match self.signalfd.read_signal() {
            Ok(Some(ref sig)) => Err(Self::err_for_signo(sig.ssi_signo as i32)),
            Ok(None) => Ok(EventLoopFlow::Continue),
            Err(e) => Err(e.into()),
        }
    }

    fn handle_listener_input(&self) -> ElResult<UnixStream> {
        let client_stream = match self.listener.accept() {
            Ok((client_stream, _)) => client_stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(EventLoopFlow::Continue),
            Err(e) => return Err(e),
        };

        #[cfg(feature = "forking")]
        if self.options.fork_mode {
            use crate::forking::{ForkResult, double_fork};
            #[allow(unsafe_code)]
            return match unsafe { double_fork() } {
                // The child runs as if in single_mode:
                Ok(ForkResult::Child) => Ok(EventLoopFlow::Return(client_stream)),
                // The parent continues its event loop:
                Ok(ForkResult::Parent { .. }) => Ok(EventLoopFlow::Continue),
                Err(e) => Err(e.into()),
            };
        }

        if self.options.single_mode {
            // let the top-level in setup run the client session after the socket and epoll fds are closed:
            Ok(EventLoopFlow::Return(client_stream))
        } else {
            // spawn the client session and allow the event handler to continue:
            self.spawn_session(client_stream);
            Ok(EventLoopFlow::Continue)
        }
    }
}

impl Drop for SocketEventHandler {
    fn drop(&mut self) { let _ = SigSet::all().thread_unblock(); }
}

impl EventHandler for SocketEventHandler {
    type ResultType = UnixStream;

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)> {
        let flags = EventFlags::IN; // level triggered
        let mut v = Vec::new();
        v.push((self.listener.as_fd(), flags));
        v.push((self.signalfd.as_fd(), flags));
        if let Some(ref server_stream) = self.server_stream {
            v.push((server_stream.as_fd(), flags));
            // EventFlags::empty() also works, but then we only get the HUP, not any input
        }
        v.into_iter()
    }

    fn handle_input(&mut self, fd_index: usize) -> ElResult<UnixStream> {
        match fd_index {
            0 => self.handle_listener_input(),
            1 => self.handle_signal_input(),
            2 => Err(Error::other("Server terminated connection")), // for -w option
            _ => unreachable!(),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn handle_output(&mut self, _fd_index: usize) -> ElResult<UnixStream> {
        unreachable!("We didn't ask for no output!")
    }
}
