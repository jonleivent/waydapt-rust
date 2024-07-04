#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use crate::buffers::{InBuffer, OutBuffer};
use crate::crate_traits::EventHandler;
use crate::for_handlers::{SessionInitHandler, SessionInitInfo};
use crate::input_handler::WaydaptInputHandler;
use crate::postparse::ActiveInterfaces;
use crate::streams::IOStream;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
pub(crate) struct Session<'a> {
    // I don't think we want the bufs to own the streams, because there are 4 bufs and only2
    // streams.  Instead, we should own the streams and the buffs should borrow them.  TBD
    in_buffers: [InBuffer; 2],
    out_buffers: [OutBuffer<IOStream>; 2],
    messenger: WaydaptInputHandler<'a>,
}

impl<'a> Session<'a> {
    pub(crate) fn new(init_info: &WaydaptSessionInitInfo, streams: [UnixStream; 2]) -> Self {
        let mut s = Self {
            in_buffers: Default::default(),
            out_buffers: streams.map(IOStream::new).map(OutBuffer::new),
            messenger: WaydaptInputHandler::new(init_info),
        };
        s.out_buffers
            .iter_mut()
            .for_each(|b| b.flush_every_send = init_info.options.flush_every_send);
        s
    }
}

impl<'a> EventHandler for Session<'a> {
    fn fds_to_monitor(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
        self.out_buffers.iter().map(|b| b.get_stream().as_fd())
    }

    fn handle_input(&mut self, index: usize) -> IoResult<()> {
        // By trying receive repeatedly until there's nothing left, we can use edge triggered IN
        // events, which may give higher performance:
        // https://thelinuxcode.com/epoll-7-c-function/
        let mut receive_count = 0u32;
        while self.in_buffers[index].receive(self.out_buffers[index].get_stream_mut())? > 0 {
            receive_count += 1;
            // We expect that at least one whole msg is received.  But we can handle things if
            // that's not the case.
            let mut msg_count = 0u32;
            while let Some((msg, fds)) = self.in_buffers[index].try_pop() {
                msg_count += 1;

                // We need to pass index into handle so it knows whether the msg is a request or event.
                self.messenger.handle(index, msg, fds, &mut self.out_buffers)?;
            }
            debug_assert!(msg_count > 0);
        }
        if receive_count > 0 {
            // If we received anything, we probably generated output.  Usually that will happen
            // on the opposite outbuf (1-index), but in the future when waydapt can originate
            // its own messages, it could be either or both.
            for b in &mut self.out_buffers {
                // Force flush because we don't know when we will be back here, so waiting to
                // flush any part might starve the receiver.
                b.flush(/*force:*/ true)?;
            }
        }
        Ok(())
    }

    fn handle_output(&mut self, index: usize) -> IoResult<()> {
        // If we don't force the flush to include the last partial chunk, what will cause that
        // last partial chunk to get sent?  Maybe nothing.  So we have to force it here.
        let amount = self.out_buffers[index].flush(/*force:*/ true)?;
        // should we check the amount flushed?
        debug_assert!(amount > 0);
        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) struct WaydaptSessionInitInfo {
    pub(crate) ucred: rustix::net::UCred,
    pub(crate) active_interfaces: &'static ActiveInterfaces,
    pub(crate) options: &'static crate::setup::WaydaptOptions,
}

impl SessionInitInfo for WaydaptSessionInitInfo {
    fn ucred(&self) -> rustix::net::UCred {
        self.ucred
    }

    fn get_active_interfaces(&self) -> &'static ActiveInterfaces {
        self.active_interfaces
    }
}

// For errors that should kill off the process even if in multithreaded mode, call
// multithread_exit if options.fork_sessions is false (which indicates multithreaded mode).
// Otherwise panic or quivalent (unwrap).  Everything in here should just panic:
pub(crate) fn client_session(
    options: &'static crate::setup::WaydaptOptions, active_interfaces: &'static ActiveInterfaces,
    session_handlers: &VecDeque<SessionInitHandler>, client_stream: UnixStream,
) {
    use crate::terminator::SessionTerminator;
    use rustix::net::sockopt::get_socket_peercred;

    // Consider a case where the wayland server's socket was deleted.  That should only prevent
    // future clients from connecting, it should not cause existing clients to exit.  So unwrap
    // instead of multithread_exit:
    let server_stream = UnixStream::connect(&options.server_socket_path).unwrap();

    // When would get_socket_peercred ever fail, given that we know the arg is correct?
    // Probably never.  Does it matter then how we handle it?:
    let ucred = get_socket_peercred(&client_stream).unwrap();
    let init_info = WaydaptSessionInitInfo { ucred, active_interfaces, options };

    // options.terminate can only be Some(duration) if options.fork_sessions is false, meaning we
    // are in multi-threaded mode:
    #[forbid(let_underscore_drop)]
    let _st = options.terminate.map(SessionTerminator::new);

    session_handlers.iter().for_each(|h| h(&init_info));

    let mut session = Session::new(&init_info, [client_stream, server_stream]);

    crate::event_loop::event_loop(&mut session).unwrap();
}
