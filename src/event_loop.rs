#![forbid(unsafe_code)]

use epoll::{CreateFlags, Event, EventData, EventFlags, EventVec};
use rustix::event::epoll;
use std::io::Result as IoResult;
use std::os::fd::BorrowedFd;

pub(crate) trait EventLoop {
    fn fds(&self) -> impl Iterator<Item = BorrowedFd<'_>>;
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

    // The rest of these shouldn't be overridden.  Should we move them out of the trait?

    fn handle(&mut self, fd_index: usize, flags: EventFlags) -> IoResult<()> {
        // If we do input and output on the same fds, which should we do first if both
        // types of events come in together?  Probably output, as that happens fastest and
        // doesn't have to wait on handler execution.
        if flags.contains(EventFlags::OUT) {
            self.handle_output(fd_index)?;
        }
        if flags.contains(EventFlags::IN) {
            self.handle_input(fd_index)?;
        }
        if !flags.difference(EventFlags::IN | EventFlags::OUT).is_empty() {
            self.handle_error(fd_index, flags)?;
        }
        Ok(())
    }

    fn event_loop(&mut self) -> IoResult<()> {
        let epoll_fd = epoll::create(CreateFlags::CLOEXEC)?;
        let fds = self.fds();
        // If we edge-trigger output (and we have to), and we combine input and output events,
        // then we have to edge-trigger input.
        let flags = EventFlags::IN | EventFlags::OUT | EventFlags::ET;
        let mut count = 0;
        for fd in fds {
            let data = EventData::new_u64(count);
            epoll::add(&epoll_fd, fd, data, flags)?;
            count += 1;
        }
        let mut events = EventVec::with_capacity(count as usize); // would longer help?
        loop {
            epoll::wait(&epoll_fd, &mut events, 0)?;
            if events.is_empty() {
                // should never occur with 0 timeout
                panic!("Got 0 events from epoll::wait");
            }
            for Event { flags, data } in events.iter() {
                let i = data.u64() as usize;
                self.handle(i, flags)?;
            }
            events.clear(); // may not be needed
        }
    }
}
