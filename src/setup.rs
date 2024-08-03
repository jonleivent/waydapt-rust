#![warn(clippy::pedantic)]
use crate::for_handlers::{
    AddHandler, AddHandlerError, InitHandlersFun, MessageHandler, SessionInitHandler,
};
use crate::forking::{daemonize, double_fork, ForkResult};
use crate::listener::SocketListener;
use crate::multithread_exit::{multithread_exit, multithread_exit_handler, ExitCode};
use crate::postparse::{active_interfaces, ActiveInterfaces};
use crate::protocol::{Interface, Message};
use crate::session::client_session;
use getopts::{Matches, Options, ParsingStyle};
use std::collections::{HashMap, VecDeque};
use std::env::Args;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct WaydaptOptions {
    pub(crate) fork_sessions: bool,
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
}

impl WaydaptOptions {
    fn new(matches: &Matches) -> &'static Self {
        let wo = Box::leak(Box::new(WaydaptOptions {
            fork_sessions: matches.opt_present("c"),
            terminate_after: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
            flush_every_send: matches.opt_present("f"),
            debug_level: match std::env::var("WAYLAND_DEBUG").as_ref().map(String::as_str) {
                Ok("1") => 1,
                Ok("client") => 2,
                Ok("server") => 3,
                _ => 0,
            },
        }));

        assert!(
            !(wo.terminate_after.is_some() && wo.fork_sessions),
            "-t and -c cannot be used together"
        );
        wo
    }
}

// Construct a single iterator over all protocol files and protocol directories
fn protocol_file_iter<'a>(
    files: &'a [String], dirs: &'a [String],
) -> impl Iterator<Item = PathBuf> + 'a {
    files.iter().map(PathBuf::from).chain(dirs.iter().flat_map(|d| {
        std::fs::read_dir(d).unwrap().filter_map(|f| {
            let f = f.unwrap().path();
            match f.extension() {
                Some(e) if e.to_ascii_lowercase() == "xml" => Some(f),
                _ => None,
            }
        })
    }))
}

fn globals_and_handlers(
    matches: &Matches, all_args: &mut Args, init_handlers: &[(&str, InitHandlersFun)],
) -> (&'static ActiveInterfaces, &'static SessionHandlers) {
    let protocol_files = matches.opt_strs("p");
    let protocol_dirs = matches.opt_strs("P");
    // This iterator produces all protocol files, individuals first, then dirs.  It assumes that
    // an individual file is always a protocol file even if it does not have the .xml extension,
    // but it filters directory content for only files that have the .xml extension:
    let all_protocol_files = protocol_file_iter(&protocol_files, &protocol_dirs);

    let globals_filename =
        matches.opt_str("g").expect("-g required globals file option is missing");
    let active_interfaces = active_interfaces(all_protocol_files, &globals_filename);

    // TBD: should we pass active_interfaces to each init_handler so it can examine which interfaces
    // and messages are active?  If not, then it might be the case that an init_handler tries to add
    // a handler to an inactive message.  We could even have the init_handler search through
    // interfaces and messages to see what it needs to do.  Alternatively, we could have add_handler
    // return a Result that says if the interface is missing or inactive, or the message is missing
    // or inactive.
    let all_handlers = get_all_handlers(all_args, init_handlers, active_interfaces);

    all_handlers.link_with_messages();

    // Dump the protocol and hanlder info if asked:
    if let Some(protocol_output_filename) = matches.opt_str("o") {
        use std::{fs::File, io::BufWriter};
        let mut out = BufWriter::new(File::create(protocol_output_filename).unwrap());
        active_interfaces.dump(&mut out).unwrap();
    }

    (active_interfaces, &all_handlers.session_handlers)
}

fn start_listening(matches: &Matches) -> SocketListener {
    use rustix::fs::{flock, FlockOperation};
    let display_name = &matches.opt_str("d").unwrap_or("waydapt-0".into()).into();
    let listener = SocketListener::new(display_name);

    // Now that the socket is ready, we can use either or both of two ways to notify/allow clients
    // to request sessions: unlock the anti-lock fd and daemonizing this process.  Daemonizing is
    // the easiest, but it requires that the startup scripts be set up with clients run right after
    // the waydapt in the same script.  The anti-lock fd can be used to coordinate startup in more
    // complex environments, such as when the waydapt starts in its own sandbox (where daeminizing
    // won't help vs. processes outside, unless the sandbox wrapper itself is smart enough to
    // daemonize when its child does).

    // If we've been given an anti-lock fd, unlock it now to allow clients waiting on it to start:
    if let Some(anti_lock_fd) = matches.opt_str("a") {
        let raw = anti_lock_fd.parse().expect("-a fd : must be an int");
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        // check that the fd is really something we own:
        assert!(
            rustix::fs::fstat(&owned).is_ok(),
            "Anti-lock (-a) fd={raw} does not correspond to an open file or dir"
        );
        flock(owned, FlockOperation::Unlock).unwrap();
    };

    // If we've been told to daemonize, do so to allow subsequent clients to start that were waiting
    // for this process to exit:
    if matches.opt_present("z") {
        unsafe { daemonize() }.unwrap();
        rustix::process::setsid().unwrap();
    }

    listener
}

fn get_all_handlers(
    all_args: &mut Args, init_handlers: &[(&str, InitHandlersFun)],
    active_interfaces: &'static ActiveInterfaces,
) -> &'static mut AllHandlers {
    // add handlers based on what's left in all_args iterator
    // the handlers are compiled in statically in Rust - but how do we introspect to find them?
    // We could use a build script to search among the files for particularly named functions in modules.
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html
    //
    // can build scripts operate across multiple crates?  Or do they each need their own?

    let all_handlers: &'static mut AllHandlers =
        Box::leak(Box::new(AllHandlers::new(active_interfaces)));
    all_handlers.mod_name = "<builtin>";
    crate::builtin::add_builtin_handlers(all_handlers);

    // Call init handlers for modules in the order that their names appear on the command line.  The
    // remainder of all_args will be name args -- name args -- ...., so we need to break it up into
    // portions using take_while.
    loop {
        let Some(handler_mod_name) = all_args.next() else { break };
        let Some((_, handler_init)) = init_handlers.iter().find(|(n, _)| n == &handler_mod_name)
        else {
            panic!("{handler_mod_name} does not have a handler init function");
        };
        // this init handler gets the next sequence of args up to the next --
        let handler_args = all_args.take_while(|a| a != "--").collect::<Vec<_>>();
        all_handlers.mod_name = Box::leak(Box::new(handler_mod_name)).as_str();
        handler_init(&handler_args, all_handlers);
    }
    all_handlers
}

type RInterface = &'static Interface<'static>;
type RMessage = &'static Message<'static>;

type HandlerMap = HashMap<&'static str, (RMessage, VecDeque<(&'static str, MessageHandler)>)>;

#[derive(Default, Debug)]
struct InterfaceHandlers {
    request_handlers: HandlerMap,
    event_handlers: HandlerMap,
}

type SessionHandlers = VecDeque<SessionInitHandler>;

struct AllHandlers {
    message_handlers: HashMap<&'static str, (RInterface, InterfaceHandlers)>,
    session_handlers: SessionHandlers,
    mod_name: &'static str,
    active_interfaces: &'static ActiveInterfaces,
}

impl AllHandlers {
    #![allow(clippy::default_trait_access)]
    fn new(active_interfaces: &'static ActiveInterfaces) -> Self {
        Self {
            message_handlers: Default::default(),
            session_handlers: Default::default(),
            mod_name: "",
            active_interfaces,
        }
    }

    fn link_with_messages(&mut self) {
        // Set the Message.handlers fields
        for (_, (_interface, mut interface_handlers)) in self.message_handlers.drain() {
            for (_, (request, request_handlers)) in interface_handlers.request_handlers.drain() {
                request.handlers.set(request_handlers).expect("should only be set once");
            }
            for (_, (event, event_handlers)) in interface_handlers.event_handlers.drain() {
                event.handlers.set(event_handlers).expect("should only be set once");
            }
        }
    }

    fn get_handlers<const IS_REQUEST: bool>(
        &mut self, interface_name: &'static str, msg_name: &'static str,
    ) -> Result<&mut VecDeque<(&'static str, MessageHandler)>, AddHandlerError> {
        use std::collections::hash_map::Entry;
        let entry = self.message_handlers.entry(interface_name);
        let (interface, ih) = match entry {
            Entry::Vacant(ve) => {
                if let Some(interface) = self.active_interfaces.maybe_get_interface(interface_name)
                {
                    ve.insert((interface, Default::default()))
                } else {
                    return Err(AddHandlerError::NoSuchInterface);
                }
            }
            Entry::Occupied(oe) => oe.into_mut(),
        };
        let entry = if IS_REQUEST {
            ih.request_handlers.entry(msg_name)
        } else {
            ih.event_handlers.entry(msg_name)
        };
        let (_message, msg_handlers) = match entry {
            Entry::Vacant(ve) => {
                let (maybe_message, err) = if IS_REQUEST {
                    (interface.get_request_by_name(msg_name), AddHandlerError::NoSuchRequest)
                } else {
                    (interface.get_event_by_name(msg_name), AddHandlerError::NoSuchEvent)
                };
                if let Some(message) = maybe_message {
                    ve.insert((message, Default::default()))
                } else {
                    return Err(err);
                }
            }
            Entry::Occupied(oe) => oe.into_mut(),
        };
        Ok(msg_handlers)
    }
}

impl AddHandler for AllHandlers {
    fn request_push_front(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<true>(interface_name, request_name)?;
        handlers.push_front((mod_name, handler));
        Ok(())
    }
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<true>(interface_name, request_name)?;
        handlers.push_back((mod_name, handler));
        Ok(())
    }
    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<false>(interface_name, event_name)?;
        handlers.push_front((mod_name, handler));
        Ok(())
    }
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<false>(interface_name, event_name)?;
        handlers.push_back((mod_name, handler));
        Ok(())
    }
    fn session_push_front(&mut self, handler: SessionInitHandler) {
        self.session_handlers.push_front(handler);
    }
    fn session_push_back(&mut self, handler: SessionInitHandler) {
        self.session_handlers.push_back(handler);
    }
}

fn get_options() -> Options {
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
    opts
}

pub(crate) fn startup(init_handlers: &[(&str, InitHandlersFun)]) -> ExitCode {
    let all_args = &mut std::env::args();
    let program = all_args.next().unwrap();
    let our_args = all_args.take_while(|a| a != "--");
    let opts = get_options();
    let matches = opts.parse(our_args).unwrap();

    if matches.opt_present("h") {
        let brief = format!("Usage: {program} [options] -- ...dlls-and-their-options...");
        print!("{}", opts.usage(&brief));
        return ExitCode::SUCCESS;
    }

    // gather options that can be used elsewhere:
    let options = WaydaptOptions::new(&matches);

    // do all protocol and globals parsing, and handler linkups:
    let (active_interfaces, session_handlers) =
        globals_and_handlers(&matches, all_args, init_handlers);

    // Start listening on the socket for our clients:
    let listener = start_listening(&matches);

    let accept = || accept_clients(listener, options, active_interfaces, session_handlers);
    // If we're going to fork client sessions, then we don't need any special multi-threaded
    // exit magic (to enable other threads to end the process in an orderly fashion, with
    // destructors).  Otherwise, we do:
    if options.fork_sessions {
        accept(); // accept_clients in this thread
        ExitCode::SUCCESS
    } else {
        // accept clients in second thread, so that this main thread can wait:
        std::thread::spawn(accept);
        // Wait for any thread to report a problem, or exit due to the terminate option:
        multithread_exit_handler()
    }
}

fn accept_clients(
    listener: SocketListener, options: &'static WaydaptOptions,
    interfaces: &'static ActiveInterfaces, handlers: &'static VecDeque<SessionInitHandler>,
) {
    let fork_sessions = options.fork_sessions;
    for client_stream in listener.incoming() {
        let Ok(client_stream) = client_stream else {
            eprintln!("Client listener error: {client_stream:?}");
            if !fork_sessions {
                // we are in thread 2, so panicking here does no good.  Instead, tell the main
                // thread to exit, and panic if that doesn't work:
                multithread_exit(ExitCode::FAILURE);
            }
            panic!();
        };
        let session = move || client_session(options, interfaces, handlers, &client_stream);

        if fork_sessions {
            // we are in the main thread (because fork_sessions), so unwrap works:
            let ForkResult::Child = unsafe { double_fork() }.unwrap() else { continue };
            // We need to drop the listener (but not remove_file at the corresponding paths)
            // because as the child, we have no need for them, but they won't drop on their own:
            listener.drop_without_removes();
            return session();
        }
        // dropping the returned JoinHandle detaches the thread, which is what we want
        std::thread::spawn(session);
    }
}
