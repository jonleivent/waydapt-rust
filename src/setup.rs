#![allow(unused)]

mod forking {
    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::fork;
    pub(crate) use nix::unistd::ForkResult;

    pub(crate) unsafe fn double_fork() -> nix::Result<ForkResult> {
        match unsafe { fork() } {
            Ok(ForkResult::Child) => match unsafe { fork() } {
                c @ Ok(ForkResult::Child) => c,
                Ok(ForkResult::Parent { .. }) => libc::_exit(0),
                Err(_) => libc::_exit(-1),
            },
            p @ Ok(ForkResult::Parent { child, .. }) => {
                waitpid(Some(child), Some(WaitPidFlag::empty()))?;
                p
            }
            e @ Err(_) => e,
        }
    }

    pub(crate) unsafe fn daemonize() -> nix::Result<ForkResult> {
        match unsafe { fork() } {
            Ok(ForkResult::Parent { .. }) => libc::_exit(0),
            r => r,
        }
    }
}

mod startup {
    use super::forking::*;
    use crate::for_handlers::*;
    use crate::postparse;
    use getopts::{Options, ParsingStyle};
    use std::collections::{HashMap, VecDeque};
    use std::io::Result as IoResult;
    use std::os::unix::io::{BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    #[derive(Debug)]
    struct WaydaptOptions {
        anti_lock_fd: Option<i32>,
        fork_children: bool,
        wayland_socket_name: PathBuf,
        terminate: Option<u32>,
        daemonize: bool,
        debug_level: u32,
        flush_every_send: bool,
        server_socket_path: PathBuf,
    }

    #[derive(Default, Debug)]
    struct InterfaceHandlers {
        request_handlers: HashMap<&'static str, VecDeque<MessageHandler>>,
        event_handlers: HashMap<&'static str, VecDeque<MessageHandler>>,
    }

    #[derive(Debug)]
    struct AllHandlers {
        message_handlers: HashMap<&'static str, InterfaceHandlers>,
        session_handlers: VecDeque<SessionInitHandler>,
    }

    impl AllHandlers {
        fn new() -> Self {
            Self { message_handlers: Default::default(), session_handlers: Default::default() }
        }
    }

    impl AddHandler for AllHandlers {
        fn request_push_front(
            &mut self, interface_name: &'static str, request_name: &'static str,
            handler: MessageHandler,
        ) {
            self.message_handlers
                .entry(interface_name)
                .or_default()
                .request_handlers
                .entry(request_name)
                .or_default()
                .push_front(handler);
        }
        fn request_push_back(
            &mut self, interface_name: &'static str, request_name: &'static str,
            handler: MessageHandler,
        ) {
            self.message_handlers
                .entry(interface_name)
                .or_default()
                .request_handlers
                .entry(request_name)
                .or_default()
                .push_back(handler);
        }
        fn event_push_front(
            &mut self, interface_name: &'static str, event_name: &'static str,
            handler: MessageHandler,
        ) {
            self.message_handlers
                .entry(interface_name)
                .or_default()
                .event_handlers
                .entry(event_name)
                .or_default()
                .push_front(handler);
        }
        fn event_push_back(
            &mut self, interface_name: &'static str, event_name: &'static str,
            handler: MessageHandler,
        ) {
            self.message_handlers
                .entry(interface_name)
                .or_default()
                .event_handlers
                .entry(event_name)
                .or_default()
                .push_back(handler);
        }
        fn session_push_front(&mut self, handler: SessionInitHandler) {
            self.session_handlers.push_front(handler);
        }
        fn session_push_back(&mut self, handler: SessionInitHandler) {
            self.session_handlers.push_back(handler);
        }
    }

    trait HandlerInit {
        fn setup_handlers(&self, args: &[String]);
    }

    fn notdd(s: &String) -> bool {
        s != "--"
    }

    fn startup() {
        let mut all_args = std::env::args();
        let program = all_args.next().unwrap();
        let mut our_args = all_args.take_while(notdd);
        let mut opts = Options::new();
        opts.parsing_style(ParsingStyle::StopAtFirstFree);
        opts.optopt(
            "a",
            "antilock",
            "anti lock file descriptor for syncing client startup with a ready socket",
            "FILE-DESCRIPTOR",
        );
        opts.optflag("c", "childprocs", "child sessions are processes instead of threads");
        opts.optopt("d", "display", "the name of the Wayland display socket to create", "NAME");
        opts.optflag("f", "flushsends", "flush every message send, instead of buffering them");
        opts.reqopt(
            "g",
            "globals",
            "file one allowed global interface and max version per line",
            "FILE",
        );
        opts.optflag("h", "help", "print this help");
        opts.optopt("o", "output", "dump processed protocol and handler info to file", "FILE");
        opts.optmulti("p", "protofile", "a protocol XML file (can appear multiple times)", "FILE");
        opts.optmulti(
            "P",
            "protodir",
            "a directory of protocol XML files (can appear multiple times)",
            "DIR",
        );
        opts.optopt(
            "t",
            "terminate",
            "terminate after last client and no others for secs (can't be used with -c)",
            "SECS",
        );
        opts.optflag("z", "daemonize", "daemonize waydapt when its socket is ready");

        let matches = opts.parse(our_args).unwrap();

        if matches.opt_present("h") {
            let brief = format!("Usage: {program} [options] -- ...dlls-and-their-options...");
            print!("{}", opts.usage(&brief));
            return;
        }

        // gather options that are used elsewhere:
        let waydapt_options = WaydaptOptions {
            anti_lock_fd: matches
                .opt_str("a")
                .map(|s| s.parse::<i32>().expect("-a fd : must be an int")),
            fork_children: matches.opt_present("c"),
            wayland_socket_name: matches.opt_str("d").unwrap_or("waydapt-0".to_string()).into(),
            terminate: matches.opt_str("t").map(|t| t.parse::<u32>().unwrap()),
            daemonize: matches.opt_present("z"),
            flush_every_send: matches.opt_present("f"),
            debug_level: if let Ok(d) = std::env::var("WAYLAND_DEBUG") {
                if d == "1" {
                    1
                } else if d == "client" {
                    2
                } else if d == "server" {
                    3
                } else {
                    0
                }
            } else {
                0
            },
            server_socket_path: get_server_socket_path(),
        };

        // options that are used here:
        let globals_filename = matches.opt_str("g").unwrap();
        let protocol_output_filename = matches.opt_str("o");
        let protocol_files = matches.opt_strs("p");
        let protocol_dirs = matches.opt_strs("P");

        if waydapt_options.terminate.is_some() && waydapt_options.fork_children {
            panic!("-t and -c cannot be used together");
        }

        let mut all_protocol_files = protocol_files.iter().map(std::path::PathBuf::from).chain(
            protocol_dirs.iter().flat_map(|d| {
                std::fs::read_dir(d).unwrap().filter_map(|f| {
                    let f = f.unwrap().path();
                    match f.extension() {
                        Some(e) if e.to_ascii_lowercase() == "xml" => Some(f),
                        _ => None,
                    }
                })
            }),
        );

        let active_interfaces = postparse::active_interfaces(all_protocol_files, &globals_filename);

        // add handlers based on what's left in all_args iterator
        // the handlers are compiled in statically in Rust - but how do we introspect to find them?
        // We could use a build script to search among the files for particularly named functions in modules.
        // https://doc.rust-lang.org/cargo/reference/build-scripts.html
        //
        // can build scripts operate across multiple crates?  Or do they each need their own?
        //
        // For now, we should just pretend that somehow we have a vector of init_handlers to call.
        // But how are they sorted, and how do they correspond to the remainder of all_args?  Maybe
        // what we have is instead of a vector, a map (hashmap) from names (strings) to init
        // handlers - and we traverse it based on the remainder of all_args.

        let handler_map: std::collections::HashMap<String, InitHandlersFun> = { todo!() };

        // the remainder of all_args will be name args -- name args -- ...., so we need to break it up into portions using take_while.

        let mut all_handlers = AllHandlers::new();

        // Process handler modules in the order that their names appear on the command line
        while let Some(handler_mod_name) = all_args.next() {
            let Some(handler_init) = handler_map.get(&handler_mod_name) else {
                panic!("{handler_mod_name} does not have a handler init function");
            };
            let handler_args = all_args.take_while(notdd).collect::<Vec<_>>();
            handler_init(&handler_args, &mut all_handlers);
        }

        // link the handlers to their messages
        for (interface_name, interface_handlers) in all_handlers.message_handlers.iter() {
            let Some(active_interface) = active_interfaces.maybe_get_interface(interface_name)
            else {
                continue;
            };
            for (name, request_handlers) in interface_handlers.request_handlers.drain() {
                if let Some(request) = active_interface.requests.iter().find(|m| m.name == *name) {
                    request.handlers.set(request_handlers).expect("should only be set once");
                }
            }
            for (name, event_handlers) in interface_handlers.event_handlers.drain() {
                if let Some(event) = active_interface.events.iter().find(|m| m.name == *name) {
                    event.handlers.set(event_handlers).expect("should only be set once");
                }
            }
        }

        // How do we provide global access to the session init handlers?

        if let Some(protocol_output_filename) = protocol_output_filename {
            // output the protocol.
        }

        accept_clients(&waydapt_options, &all_handlers.session_handlers);
    }

    fn create_socket(waydapt_options: &WaydaptOptions) -> (OwnedFd, OwnedFd) {
        // see wayland-rs/wayland-server/src/socket.rs
        todo!()
    }

    fn get_server_socket_path() -> PathBuf {
        use std::env;
        let mut socket_path: PathBuf =
            env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0".into()).into();
        if !socket_path.is_absolute() {
            let Ok(xdg_runtime_dir) = env::var("XDG_RUNTIME_DIR") else {
                panic!("XDG_RUNTIME_DIR is not set")
            };
            let xdg_runtime_path = PathBuf::from(xdg_runtime_dir);
            if !xdg_runtime_path.is_absolute() {
                panic!("XDG_RUNTIME_DIR is not absolute: {xdg_runtime_path:?}")
            }
            socket_path = xdg_runtime_path.join(socket_path);
        }
        socket_path
    }

    fn client_session(
        waydapt_options: &WaydaptOptions, init_handlers: &VecDeque<SessionInitHandler>,
        clientfd: OwnedFd,
    ) -> IoResult<()> {
        use rustix::net::sockopt::get_socket_peercred;
        use rustix::net::UCred;
        use std::process::exit;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::thread::sleep;
        use std::time::Duration;

        static CLIENT_SESSION_COUNT: AtomicU32 = AtomicU32::new(0);
        static CLIENT_SESSION_TOTAL: AtomicU32 = AtomicU32::new(0);

        CLIENT_SESSION_COUNT.fetch_add(1, Ordering::Acquire);
        CLIENT_SESSION_TOTAL.fetch_add(1, Ordering::Relaxed);

        let UCred { pid, uid, gid } = get_socket_peercred(&clientfd)?;

        let server_stream = UnixStream::connect(&waydapt_options.server_socket_path)?;
        let client_stream = UnixStream::from(clientfd);

        // but we need 4 streams- 2 on each side, input and output.  In C waydapt, we only had input
        // streams in the epoll.  Or are they bidirectional?  It looks like they are bidirectional,
        // in which case we should modify the session and event loop in buffers to take only 2 fds
        // and apply input and output to both. Or, can we leave it as is, and give the event loop 2
        // pairs with separate input and output events for each?  But then we have a problem of
        // ownership.  In buffers, we have InUnixStream and OutUnixStream, both claiming ownership.
        // We could use UnixStream::try_clone.  Or we could implement InStream and OutStream on the
        // same owned UnixStream.  Why not?  They both need a cmsg_space field, and can share it
        // because it can't be used at the same time for both direcitons.

        let session_info = todo!();

        init_handlers.iter().map(|h| h(session_info));

        todo!();

        let errno = todo!();

        if let Some(terminate_after) = waydapt_options.terminate {
            if CLIENT_SESSION_COUNT.fetch_sub(1, Ordering::Release) == 0 {
                let saved_client_session_total = CLIENT_SESSION_TOTAL.load(Ordering::Relaxed);
                sleep(Duration::from_secs(terminate_after.into()));
                if saved_client_session_total == CLIENT_SESSION_TOTAL.load(Ordering::Relaxed) {
                    exit(errno)
                }
            }
        }

        Ok(())
    }

    fn accept_clients(
        waydapt_options: &WaydaptOptions, init_handlers: &VecDeque<SessionInitHandler>,
    ) -> IoResult<()> {
        // Instead of fds, create_socket should return something like ListeningSocket in
        // wayland-rs/wayland-server/src/socket.rs.  It looks like we want pretty much that exact
        // code.
        let (socketfd, lockfd) = create_socket(waydapt_options);

        if let Some(anti_lock_raw_fd) = waydapt_options.anti_lock_fd {
            let anti_lock_fd = unsafe { OwnedFd::from_raw_fd(anti_lock_raw_fd) };
            rustix::fs::flock(&anti_lock_fd, rustix::fs::FlockOperation::Unlock)?;
            // fd closed here
            drop(anti_lock_fd);
        }

        if waydapt_options.daemonize {
            unsafe { daemonize()? };
            rustix::process::setsid()?;
        }
        loop {
            let clientfd = todo!();

            if waydapt_options.fork_children {
                if let ForkResult::Child = unsafe { double_fork()? } {
                    drop(socketfd);
                    drop(lockfd);
                    return client_session(waydapt_options, init_handlers, clientfd);
                }
                // would happen automatically when continuing the loop, because it wasn't moved:
                drop(clientfd);
                continue;
            }
            // dropping the returned JoinHandle detaches the thread, which is what we want:
            std::thread::spawn(|| {
                // It may not be enough to unwrap.  See:
                // https://stackoverflow.com/questions/35988775/how-can-i-cause-a-panic-on-a-thread-to-immediately-end-the-main-thread
                // - maybe setting panic to 'abort' in Cargo.toml
                //
                // But we want to differentiate between cases where we do want to abort the whole
                // waydapt vs. those that should only kill off the offending thread.  The errors
                // generated from our code in client_session should abort the process, but panics
                // from within handlers?  Maybe we need some enum to differentiate the two cases?
                // Or a trait?  The problem with traits is that you can't test if something has or
                // doesn't have a trait.  Let's consider errors returnd from client_session as
                // Results to be ours, so they cause an abort.  Handlers just panic, and we only
                // kill of the thread as a result.
                //
                // also, consider: https://doc.rust-lang.org/std/thread/fn.panicking.html
                client_session(waydapt_options, init_handlers, clientfd)
                    .unwrap_or_else(|_| std::process::exit(-1))
            });
        }
        Ok(())
    }
}

// How do we implement the _Atomic client_session_{count,total} scheme in Rust?  These are used
// within waydapt_client_session to determine when to exit the whole waydapt process due to the
// terminate "t" option.  But these have to be global across-thread counters.  But that doesn't mean
// they need to be global - just that they need to be passed into the threaded version of
// client_session.  But does it have to be interior mutability, or is there an atomic counter in
// Rust that we can use?  It looks like AtomicU32 has methods that are &self (not &mut self) that
// change the counter like we need.  So, this should work as statics.
//
// Alternatively, we could move the above counting into the accept_clients loop.  But we would still
// need the threads to decrement the counter.  And we would need a way to timeout of the accept.  So
// maybe that isn't worth it.
