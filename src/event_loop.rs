#![forbid(unsafe_code)]

//# This might become its own crate

use epoll::{CreateFlags, Event, EventData, EventFlags, EventVec};
use rustix::event::epoll;
use std::{io::Result as IoResult, os::fd::BorrowedFd};

pub enum EventLoopFlow<T> {
    Return(T),
    RefreshFDs, // and continue
    Continue,
}

pub type ElResult<T> = IoResult<EventLoopFlow<T>>;

pub trait EventHandler {
    type ResultType;

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)>;

    /// # Errors
    ///
    /// Propagate any IO Error
    fn handle_input(&mut self, fd_index: usize) -> ElResult<Self::ResultType>;

    /// # Errors
    ///
    /// Propagate any IO Error
    fn handle_output(&mut self, fd_index: usize) -> ElResult<Self::ResultType>;

    /// # Errors
    ///
    /// Determines error from flags.  Is allowed to return `Ok(None)` when the error can be ignored
    /// safely.
    fn handle_error(&mut self, _fd_index: usize, flags: EventFlags) -> ElResult<Self::ResultType> {
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

/// # Panics
///
/// Panics only in the unexpected case of not receiving any events from `epoll` on return from
/// `epoll::wait`
///
/// # Errors
///
/// propagates returned errors from `EventHandler` trait functions
///
pub fn event_loop<E: EventHandler>(event_handler: &mut E) -> IoResult<E::ResultType> {
    'restart: loop {
        let epoll_fd = epoll::create(CreateFlags::CLOEXEC)?;
        let mut restart = false;
        let mut count = 0;
        {
            let mut etinputs = Vec::new();
            let fds_flags = event_handler.fds_to_monitor();
            for (fd, flags) in fds_flags {
                let data = EventData::new_u64(count as u64);
                epoll::add(&epoll_fd, fd, data, flags)?;
                // keep track of edge-triggered inputs for loop below, which needs to be separate from
                // this loop to satisfy the borrow checker:
                if flags.contains(EventFlags::IN | EventFlags::ET) {
                    etinputs.push(count);
                }
                count += 1;
            }
            // for edge-triggered inputs, we may miss initial state, so try it here:
            for i in etinputs {
                match event_handler.handle_input(i)? {
                    EventLoopFlow::Return(x) => return Ok(x),
                    EventLoopFlow::RefreshFDs => restart = true,
                    EventLoopFlow::Continue => {}
                }
            }
            if restart {
                continue 'restart;
            }
        }
        debug_assert!(count > 0);
        let mut events = EventVec::with_capacity(count); // would longer help?
        loop {
            match epoll::wait(&epoll_fd, &mut events, -1) {
                Ok(()) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            // should never occur with no timeout:
            assert!(!events.is_empty(), "Got 0 events from epoll::wait");
            for Event { flags, data } in &events {
                #[allow(clippy::cast_possible_truncation)]
                let i = data.u64() as usize;
                // Since we do input and output on the same fds, we should first do output, as that
                // happens fastest and doesn't have to wait on handler execution.
                if flags.contains(EventFlags::OUT) {
                    match event_handler.handle_output(i)? {
                        EventLoopFlow::Return(x) => return Ok(x),
                        EventLoopFlow::RefreshFDs => restart = true,
                        EventLoopFlow::Continue => {}
                    }
                }
                if flags.contains(EventFlags::IN) {
                    match event_handler.handle_input(i)? {
                        EventLoopFlow::Return(x) => return Ok(x),
                        EventLoopFlow::RefreshFDs => restart = true,
                        EventLoopFlow::Continue => {}
                    }
                }
                let error_flags = flags.difference(EventFlags::IN | EventFlags::OUT);
                if !error_flags.is_empty() {
                    match event_handler.handle_error(i, error_flags)? {
                        EventLoopFlow::Return(x) => return Ok(x),
                        EventLoopFlow::RefreshFDs => restart = true,
                        EventLoopFlow::Continue => {}
                    }
                }
            }
            if restart {
                continue 'restart;
            }
            events.clear(); // may not be needed
        }
    }
}

// Note that although we use edge triggering (ET), the flags are level triggered.  Whenever an input
// event is triggered (when available input goes up from 0), unless the output kernel socket buffer
// is full at that point, we will see both IN and OUT flags for the event.  This causes us to waste
// a little time trying to resend the last send if it failed (because there wasn't enough room).
// But it is a very small amount of time we waste: almost all cases when we receive IN, we have no
// output to send.  The only time we would is if the previous sends were blocked because the kernel
// buffer was too full - and that would probably only happen if the peer was asleep or something.
//
// It would have been more beneficial if epoll::wait gave us the amount of data available in the
// event.  Well, it would need 2 amounts, one for input and one for output.  But it doesn't.
//
// The reason we need ET for OUT is that we don't want to be repeatedly given OUT events by
// epoll::wait when there is not enough space in the kernel socket buffer for us to send a complete
// chunk AND that space isn't changing.  Making it ET will mean we are only notified with an OUT
// event when the space is changing.
//
// There might not be any benefit for ET on the IN side.  We consume inputs in 4K chunks, but we do
// so repeatedly until all input is exhausted before returning to epoll::wait.
