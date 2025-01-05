#![forbid(unsafe_code)]

use rustix::event::epoll::EventFlags;

use crate::buffers::{InBuffer, OutBuffer};
use crate::crate_traits::EventHandler;
use crate::for_handlers::SessionInitInfo;
use crate::handlers::SessionHandlers;
use crate::mediator::Mediator;
use crate::postparse::ActiveInterfaces;
use crate::protocol::Interface;
use crate::setup::SharedOptions;
use crate::streams::IOStream;
use std::any::Any;
use std::io::Result as IoResult;
use std::os::unix::io::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
struct Session<'a> {
    in_buffers: [InBuffer<'a>; 2],
    out_buffers: [OutBuffer<'a>; 2],
    mediator: Mediator<'a, InitInfo>,
    group_states: Vec<(String, Box<dyn Any>)>,
    fds: [BorrowedFd<'a>; 2],
}

impl<'a> Session<'a> {
    fn new(
        init_info: &'a InitInfo, group_states: Vec<(String, Box<dyn Any>)>,
        streams: [&'a IOStream; 2],
    ) -> Self {
        let mut s = Self {
            in_buffers: streams.map(InBuffer::new),
            out_buffers: streams.map(OutBuffer::new),
            mediator: Mediator::new(init_info),
            group_states,
            fds: streams.map(AsFd::as_fd),
        };
        for b in &mut s.out_buffers {
            b.flush_every_send = init_info.options.flush_every_send;
        }
        s
    }
}

impl EventHandler for Session<'_> {
    type InputResult = ();

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)> {
        self.fds
            .into_iter()
            .zip(std::iter::repeat(EventFlags::IN | EventFlags::OUT | EventFlags::ET))
    }

    fn handle_input(&mut self, index: usize) -> IoResult<Option<()>> {
        // By trying receive repeatedly until there's nothing left, we can use edge triggered IN
        // events, which may give higher performance:
        // https://thelinuxcode.com/epoll-7-c-function/
        let (source_side, dest_side) = (index, 1 - index);
        let inbuf = &mut self.in_buffers[source_side];
        let outbuf = &mut self.out_buffers[dest_side];
        let group_states = &mut self.group_states;
        let mut received = false;

        while inbuf.receive()? > 0 {
            // We expect that at least one whole msg is received.  But we can handle things if
            // that's not the case.
            while let Some((msg, fds)) = inbuf.try_pop() {
                received = true;
                // We need to pass index into handle so it knows whether the msg is a request or
                // event.
                self.mediator.mediate(source_side, msg, fds, outbuf, group_states)?;
            }
        }

        if received {
            // Force flush because we don't know when we will be back here, so waiting to
            // flush any part might starve the receiver.
            outbuf.flush(true)?;
        }
        Ok(None)
    }

    fn handle_output(&mut self, index: usize) -> IoResult<()> {
        // If we don't force the flush to include the last partial chunk, what will cause that
        // last partial chunk to get sent?  Maybe nothing.  So we have to force it here.
        let buf = &mut self.out_buffers[index];
        buf.got_output_event();
        let _amount = buf.flush(/*force:*/ true)?;
        // should we check the amount flushed?
        //debug_assert!(amount > 0);
        Ok(())
    }
}

#[derive(Debug)]
struct InitInfo {
    ucred: Option<rustix::net::UCred>,
    active_interfaces: &'static ActiveInterfaces,
    options: &'static crate::setup::SharedOptions,
}

impl SessionInitInfo for InitInfo {
    fn ucred(&self) -> Option<rustix::net::UCred> { self.ucred }

    fn get_active_interfaces(&self) -> &'static ActiveInterfaces { self.active_interfaces }

    fn get_display(&self) -> &'static Interface<'static> { self.active_interfaces.get_display() }

    fn get_debug_level(&self) -> u32 { self.options.debug_level }
}

pub(crate) fn get_server_stream() -> UnixStream {
    let server_socket_path = crate::listener::get_server_socket_path();
    UnixStream::connect(server_socket_path).unwrap_or_else(|e| {
        panic!(
            "Cannot connect to Wayland server through socket {:?}:\n{e:?}",
            server_socket_path.as_path()
        )
    })
}

// For errors that should kill off the process even if in multithreaded mode, call
// multithread_exit if options.fork_sessions is false (which indicates multithreaded mode).
// Otherwise panic or quivalent (unwrap).  Everything in here should just panic:
pub(crate) fn client_session(
    options: &'static SharedOptions, active_interfaces: &'static ActiveInterfaces,
    session_handlers: SessionHandlers, client_stream: UnixStream,
) {
    use crate::terminator::SessionTerminator;
    use rustix::net::sockopt::get_socket_peercred;
    use std::io::ErrorKind;

    // This function will own both streams - so do the following trivial assignment for
    // client_stream to avoid clippy complaint.  The reason is that it is difficult for Session to
    // own the streams because it also needs references to those streams from its buffers, and
    // owning something while also referencing it is not Rusty.
    let client_stream = client_stream;
    let ucred = get_socket_peercred(&client_stream).ok();
    let server_stream = get_server_stream();
    let init_info = InitInfo { ucred, active_interfaces, options };

    // options.terminate can only be Some(duration) if options.fork_sessions is false, meaning we
    // are in multi-threaded mode - use it to conditionally set up a SessionTerminator that will
    // terminate the waydapt process after the last session ends plus the duration:
    #[forbid(let_underscore_drop)]
    let _st = options.terminate_after.map(SessionTerminator::new);

    let group_states =
        session_handlers.iter().map(|(n, h)| (n.clone(), h.init(&init_info))).collect::<Vec<_>>();

    let mut session = Session::new(&init_info, group_states, [&client_stream, &server_stream]);

    if let Err(e) = crate::event_loop::event_loop(&mut session) {
        let pid = ucred.map(|c| c.pid);
        match e.kind() {
            ErrorKind::ConnectionReset => eprintln!("Connection reset for {pid:?}: {e:?}"),
            ErrorKind::ConnectionAborted => eprintln!("Connection aborted for {pid:?}: {e:?}"),
            _ => eprintln!("Unexpected session error for {pid:?}: {e:?}"),
        }
    }
}
