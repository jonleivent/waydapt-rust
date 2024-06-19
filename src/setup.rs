#![allow(unused)]

use crate::for_handlers::*;
use crate::forking::*;
use crate::listener::SocketListener;
use crate::multithread_exit::*;
use crate::postparse::{active_interfaces, ActiveInterfaces};
use crate::session::client_session;
use getopts::{Options, ParsingStyle};
use std::collections::{HashMap, VecDeque};
use std::io::Result as IoResult;
use std::os::unix::io::{BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct WaydaptOptions {
    pub(crate) fork_sessions: bool,
    pub(crate) display_name: PathBuf,
    pub(crate) terminate: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
    pub(crate) server_socket_path: PathBuf,
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
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
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
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
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

pub(crate) fn startup() -> ExitCode {
    let mut all_args = std::env::args();
    let program = all_args.next().unwrap();
    // TBD: I don't know if this &mut .take_while does what we want.  We want it to remove the
    // args it takes from all_args, but does it?  Yes:
    //https://play.rust-lang.org/?version=stable&mode=debug&edition=2021&gist=c54c9e36caec05bcd59ba01050cc6cc2
    let mut our_args = (&mut all_args).take_while(|a| a != "--");
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
        return ExitCode::SUCCESS;
    }

    // gather options that are used elsewhere:
    let waydapt_options = Box::leak(Box::new(WaydaptOptions {
        fork_sessions: matches.opt_present("c"),
        display_name: matches.opt_str("d").unwrap_or("waydapt-0".to_string()).into(),
        terminate: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
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
        server_socket_path: crate::listener::get_server_socket_path(),
    }));

    // options that are used here:
    let anti_lock_raw_fd =
        matches.opt_str("a").map(|s| s.parse::<i32>().expect("-a fd : must be an int"));
    let globals_filename =
        matches.opt_str("g").expect("-g required globals file option is missing");
    let protocol_output_filename = matches.opt_str("o");
    let protocol_files = matches.opt_strs("p");
    let protocol_dirs = matches.opt_strs("P");
    let daemonize_when_socket_ready = matches.opt_present("z");

    if waydapt_options.terminate.is_some() && waydapt_options.fork_sessions {
        panic!("-t and -c cannot be used together");
    }

    let anti_lock_fd: Option<OwnedFd> = if let Some(raw) = anti_lock_raw_fd {
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        // check that the fd is really something we own:
        let Ok(_) = rustix::fs::fstat(&owned) else {
            panic!("Anti-lock (-a) fd={raw} does not correspond to an open file or dir");
            // It would be nice if we could confirm that the file is locked, but how?  Maybe we find
            // out only when we try to unlock it?
        };
        Some(owned)
    } else {
        None
    };

    // This iterator produces all protocol files, individuals first, then dirs.  It assumes that
    // an individual file is always a protocol file even if it does not have the .xml extension,
    // but it filters directory content for only files that have the .xml extension:
    let all_protocol_files = protocol_files.iter().map(std::path::PathBuf::from).chain(
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

    ////////////////////////////////////////////////////////////////////////////////

    let active_interfaces = active_interfaces(all_protocol_files, &globals_filename);

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

    let handler_map: std::collections::HashMap<String, InitHandlersFun> = get_init_handlers();

    // the remainder of all_args will be name args -- name args -- ...., so we need to break it
    // up into portions using take_while.

    let all_handlers = Box::leak(Box::new(AllHandlers::new()));

    // Call init handlers for modules in the order that their names appear on the command line:
    loop {
        let Some(handler_mod_name) = all_args.next() else { break };
        let Some(handler_init) = handler_map.get(&handler_mod_name) else {
            panic!("{handler_mod_name} does not have a handler init function");
        };
        // this init handler gets the next sequence of args up to the next --
        let handler_args = (&mut all_args).take_while(|a| a != "--").collect::<Vec<_>>();
        handler_init(&handler_args, all_handlers);
    }

    // Link the message handlers to their messages:
    for (interface_name, interface_handlers) in all_handlers.message_handlers.iter_mut() {
        let Some(active_interface) = active_interfaces.maybe_get_interface(interface_name) else {
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

    // Dump the protocol and hanlder info if asked:
    if let Some(protocol_output_filename) = protocol_output_filename {
        // output the protocol.
    }

    // Prepare the socket for our clients:
    let listener = SocketListener::new(&waydapt_options.display_name);

    // Now that the socket is ready, we can use either or both of the two signalling mechanisms
    // to allow clients to request sessions:

    // If we've been given an anti-lock fd, unlock it now to allow clients waiting on it to start:
    if let Some(owned) = anti_lock_fd {
        use rustix::fs::{flock, FlockOperation};
        flock(owned, FlockOperation::Unlock).unwrap();
    }

    // If we've been told to daemonize, do so to allow subsequent clients to start that waiting
    // for this process to exit:
    if daemonize_when_socket_ready {
        unsafe { daemonize() }.unwrap();
        rustix::process::setsid().unwrap();
    }

    let accept_clients = || {
        accept_clients(listener, waydapt_options, active_interfaces, &all_handlers.session_handlers)
    };
    // If we're going to fork client sessions, then we don't need any special multi-threaded
    // exit magic (to enable other threads to end the process in an orderly fashion, with
    // destructors).  Otherwise, we do:
    if waydapt_options.fork_sessions {
        accept_clients(); // accept_clients in this thread
        ExitCode::SUCCESS
    } else {
        std::thread::spawn(accept_clients); // accept_clients in second thread

        // Wait for any thread to report a problem, or exit due to the terminate option:
        multithread_exit_handler()
    }
}

fn accept_clients(
    listener: SocketListener, options: &'static WaydaptOptions,
    active_interfaces: &'static ActiveInterfaces,
    session_handlers: &'static VecDeque<SessionInitHandler>,
) {
    let fork_sessions = options.fork_sessions;
    for client_stream in listener.incoming() {
        let client_session = {
            match client_stream {
                Ok(client_stream) => move || {
                    client_session(options, active_interfaces, session_handlers, client_stream)
                },
                Err(e) => {
                    eprintln!("Client listener error: {e:?}");
                    if !fork_sessions {
                        // we are in thread 2, so panicking here does no good
                        multithread_exit(ExitCode::FAILURE);
                    }
                    panic!();
                }
            }
        };

        if fork_sessions {
            // we are in the main thread (because fork_sessions), so unwrap works:
            let ForkResult::Child = unsafe { double_fork() }.unwrap() else { continue };
            // We need to drop the listener (but not remove_file at the corresponding paths)
            // because as the child, we have no need for them, but they won't drop on their own:
            listener.drop_without_removes();
            return client_session();
        } else {
            // dropping the returned JoinHandle detaches the thread, which is what we want
            std::thread::spawn(client_session);
        }
    }
}

fn get_init_handlers() -> HashMap<String, InitHandlersFun> {
    todo!()
}
