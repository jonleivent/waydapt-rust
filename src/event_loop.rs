#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use crate::crate_traits::EventHandler;
use epoll::{CreateFlags, Event, EventData, EventFlags, EventVec};
use rustix::event::epoll;
use std::io::Result as IoResult;

pub(crate) fn event_loop<E: EventHandler>(event_handler: &mut E) -> IoResult<()> {
    let epoll_fd = epoll::create(CreateFlags::CLOEXEC)?;
    let fds = event_handler.fds_to_monitor();
    // If we edge-trigger output (and we have to), and we combine input and output events,
    // then we have to edge-trigger input.
    let flags = EventFlags::IN | EventFlags::OUT | EventFlags::ET;
    let mut count = 0;
    for fd in fds {
        let data = EventData::new_u64(count as u64);
        epoll::add(&epoll_fd, fd, data, flags)?;
        count += 1;
    }
    debug_assert!(count > 0);
    // Since everything is edge-triggered, we may miss initial input state, so try it here:
    for i in 0..count {
        event_handler.handle_input(i)?;
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut events = EventVec::with_capacity(count); // would longer help?
    loop {
        epoll::wait(&epoll_fd, &mut events, -1)?;
        // should never occur with no timeout:
        assert!(!events.is_empty(), "Got 0 events from epoll::wait");
        for Event { flags, data } in &events {
            #[allow(clippy::cast_possible_truncation)]
            let i = data.u64() as usize;
            // Since we do input and output on the same fds, we should first do output, as that happens
            // fastest and doesn't have to wait on handler execution.
            if flags.contains(EventFlags::OUT) {
                event_handler.handle_output(i)?;
            }
            if flags.contains(EventFlags::IN) {
                event_handler.handle_input(i)?;
            }
            if !flags.difference(EventFlags::IN | EventFlags::OUT).is_empty() {
                event_handler.handle_error(i, flags)?;
            }
        }
        events.clear(); // may not be needed
    }
}
