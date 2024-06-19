#![forbid(unsafe_code)]

use crate::buffers::*;
use crate::event_loop::*;
use crate::for_handlers::*;
use crate::input_handler::*;
use crate::postparse::ActiveInterfaces;
use crate::streams::*;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
pub(crate) struct Session<M: InputHandler> {
    // I don't think we want the bufs to own the streams, because there are 4 bufs and only2
    // streams.  Instead, we should own the streams and the buffs should borrow them.  TBD
    in_buffers: [InBuffer; 2],
    out_buffers: [OutBuffer<IOStream>; 2],
    handler: M,
}

impl<M: InputHandler> Session<M> {
    pub(crate) fn new(handler: M, streams: [UnixStream; 2]) -> Self {
        Self {
            handler,
            in_buffers: Default::default(),
            out_buffers: streams.map(IOStream::new).map(OutBuffer::new),
        }
    }

    pub(crate) fn run(&mut self) -> IoResult<()> {
        EventLoop::event_loop(self)
    }
}

impl<M: InputHandler> EventLoop for Session<M> {
    fn fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
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

                self.handler.handle(msg, fds, &mut self.out_buffers)?;
            }
            debug_assert!(msg_count > 0);
        }
        if receive_count > 0 {
            // If we received anything, we probably generated output.  Usually that will happen
            // on the opposite outbuf (1-index), but in the future when waydapt can originate
            // its own messages, it could be either or both.
            for b in self.out_buffers.iter_mut() {
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

struct WaydaptInputHandler {} // TBD

impl WaydaptInputHandler {
    pub(crate) fn new() -> Self {
        Self {}
    }
}

impl InputHandler for WaydaptInputHandler {
    fn handle<P, C>(
        &mut self,
        in_msg: &[u8],
        in_fds: &mut P,
        output: &mut [C; 2], // output to: [client,server]
    ) -> IoResult<()>
    where
        P: PopFds,
        C: OutChannel,
    {
        // The demarshalling and remarshalling, along with message handlers:
        todo!()
    }
}

struct WaydaptSessionInfo {
    ucred: rustix::net::UCred,
    active_interfaces: &'static ActiveInterfaces,
}

impl SessionInfo for WaydaptSessionInfo {
    fn ucred(&self) -> rustix::net::UCred {
        self.ucred
    }
}

// For errors that should kill off the process even if in multithreaded mode, call
// multithread_exit if options.fork_sessions is false (which indicates multithreaded mode).
// Otherwise panic or quivalent (unwrap).  Everything in here should just panic:
pub(crate) fn client_session(
    options: &crate::setup::WaydaptOptions, active_interfaces: &'static ActiveInterfaces,
    session_handlers: &VecDeque<SessionInitHandler>, client_stream: UnixStream,
) {
    use crate::event_loop::EventLoop;
    use crate::terminator::SessionTerminator;
    use rustix::net::sockopt::get_socket_peercred;

    // Consider a case where the wayland server's socket was deleted.  That should only prevent
    // future clients from connecting, it should not cause existing clients to exit.  So unwrap
    // instead of multithread_exit:
    let server_stream = UnixStream::connect(&options.server_socket_path).unwrap();

    // When would get_socket_peercred ever fail, given that we know the arg is correct?
    // Probably never.  Does it matter then how we handle it?:
    let ucred = get_socket_peercred(&client_stream).unwrap();
    let session_info = WaydaptSessionInfo { ucred, active_interfaces };
    let mut session = Session::new(WaydaptInputHandler::new(), [client_stream, server_stream]);

    // options.terminate can only be Some(duration) if options.fork_sessions is false, meaning we
    // are in multi-threaded mode:
    let _st = options.terminate.map(SessionTerminator::new);

    session_handlers.iter().for_each(|h| h(&session_info));

    session.event_loop().unwrap();
}
