use crate::addons::IHMap;
use crate::basics::Leaker;
use crate::crate_traits::Alloc;
use crate::handlers::{gather_handlers, SessionHandlers};
use crate::listener::SocketListener;
use crate::postparse::{active_interfaces, ActiveInterfaces};
use crate::session::client_session;
use crate::socket_events::SocketEventHandler;
use getopts::{Matches, Options, ParsingStyle};
use nix::sys::pthread;
use nix::sys::signal;
use rustix::fs::{flock, FlockOperation};
use signal::Signal;
use std::env::Args;
use std::io::{ErrorKind, Result as IoResult};
use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use std::{fs::File, io::BufWriter};

#[derive(Debug)]
pub(crate) struct SharedOptions {
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
    pub(crate) watch_server: bool,
    pub(crate) single_mode: bool,
}

static MAIN_THREAD: OnceLock<pthread::Pthread> = OnceLock::new();

pub(crate) fn startup(init_handlers: &IHMap) {
    let all_args = &mut std::env::args();
    let program = all_args.next().unwrap();
    let our_args = all_args.take_while(|a| a != "--");
    let opts = get_options();
    let matches = opts.parse(our_args).unwrap();

    if matches.opt_present("h") {
        let brief = format!("Usage: {program} [options] -- ...addons-and-their-options...");
        eprint!("{}", opts.usage(&brief));
        return;
    }

    if matches.opt_present("l") {
        eprint!("Available addon modules: ");
        for m in init_handlers.keys() {
            eprint!("{m} ");
        }
        eprintln!();
    }

    if matches.opt_present("v") {
        crate::version_info::print_version_info();
        return;
    }

    // gather options that can be used elsewhere:
    let options = SharedOptions::new(&matches);

    // do all protocol and globals parsing, and handler linkups:
    let (interfaces, handlers) = globals_and_handlers(&matches, all_args, init_handlers);

    // Keep global track of this main thread for signalling purposes
    MAIN_THREAD.set(pthread::pthread_self()).unwrap();

    let res = {
        // Start listening on the socket for our clients:
        let listener = start_listening(&matches);

        let mut socket_event_handler =
            SocketEventHandler::new(listener, options, interfaces, handlers);

        // accept new clients
        crate::event_loop::event_loop(&mut socket_event_handler)
    };
    match res {
        Ok(client_stream) => client_session(options, interfaces, handlers, client_stream),
        Err(e) => match (e.kind(), e.into_inner()) {
            (ErrorKind::Interrupted, Some(i)) => eprintln!("Interrupted with {i}"),
            (ErrorKind::Interrupted, None) => eprintln!("Interrupted"),
            (ErrorKind::Other, Some(i)) => eprintln!("Normal termination: {i:?}"),
            (ErrorKind::Other, None) => eprintln!("Normal termination"),
            (k, Some(i)) => panic!("{k}: {i:?}"),
            (k, None) => panic!("{k}"),
        },
    };
}

impl SharedOptions {
    fn new(matches: &Matches) -> &'static Self {
        let so = Leaker.alloc(SharedOptions {
            terminate_after: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
            flush_every_send: matches.opt_present("f"),
            debug_level: match std::env::var("WAYLAND_DEBUG").as_ref().map(String::as_str) {
                Ok("1" | "all" | "both") => 1,
                Ok("2" | "client") => 2,
                Ok("3" | "server") => 3,
                _ => 0,
            },
            watch_server: matches.opt_present("w"),
            single_mode: matches.opt_present("s"),
        });
        assert!(
            !(so.terminate_after.is_some() && so.single_mode),
            "-t and -s are mutually exclusive"
        );
        so
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
    matches: &Matches, all_args: &mut Args, init_handlers: &IHMap,
) -> (&'static ActiveInterfaces, &'static SessionHandlers) {
    let protocol_files = matches.opt_strs("p");
    let protocol_dirs = matches.opt_strs("P");
    // This iterator produces all protocol files, individuals first, then dirs.  It assumes that
    // an individual file is always a protocol file even if it does not have the .xml extension,
    // but it filters directory content for only files that have the .xml extension:
    let all_protocol_files = protocol_file_iter(&protocol_files, &protocol_dirs);

    let globals_filename =
        matches.opt_str("g").expect("-g required globals file option is missing");
    let allow_missing = matches.opt_present("m");
    let active_interfaces = active_interfaces(all_protocol_files, &globals_filename, allow_missing);

    let session_handlers = gather_handlers(all_args, init_handlers, active_interfaces);

    // Dump the protocol and handler info if asked:
    if let Some(protocol_output_filename) = matches.opt_str("o") {
        let mut out = BufWriter::new(File::create(protocol_output_filename).unwrap());
        active_interfaces.dump(&mut out).unwrap();
    }

    (active_interfaces, session_handlers)
}

fn start_listening(matches: &Matches) -> SocketListener {
    #![allow(unsafe_code)]
    let display_name = &matches.opt_str("d").unwrap_or("waydapt-0".into()).into();
    let listener = SocketListener::new(display_name);

    // If we've been given an anti-lock fd, unlock it now to allow clients waiting on it to start:
    if let Some(anti_lock_fd) = matches.opt_str("a") {
        let raw = anti_lock_fd.parse().expect("-a fd : must be an int");
        #[allow(unsafe_code)]
        let Ok(owned) = (unsafe { raw_to_owned(raw) }) else {
            panic!("Anti-lock (-a) fd={raw} does not correspond to an open file or dir");
        };
        flock(owned, FlockOperation::Unlock).unwrap();
    };

    listener
}

fn get_options() -> Options {
    let mut opts = Options::new();
    opts.parsing_style(ParsingStyle::StopAtFirstFree);
    opts.optopt(
        "a",
        "antilock",
        "file descriptor for waydapt to unlock when its socket is ready",
        "FILE-DESCRIPTOR",
    );
    opts.optopt("d", "display", "the name of the Wayland display socket to create", "NAME");
    //opts.optopt("e", "exitlock", "file to monitor for exiting when unlocked", "FILE");
    opts.optflag("f", "flushsends", "send every message immediately, instead of batching them");
    opts.optopt(
        "g",
        "globals",
        "file with one allowed global interface and max version per line",
        "FILE",
    );
    opts.optflag("h", "help", "print this help");
    opts.optflag("l", "list", "list available addon modules");
    opts.optflag("m", "allowmissing", "allow global entries that don't appear in protocol files");
    opts.optopt("o", "output", "dump processed protocol and handler info to file", "FILE");
    opts.optmulti("p", "protofile", "a protocol XML file (can appear multiple times)", "FILE");
    opts.optmulti(
        "P",
        "protodir",
        "a directory of protocol XML files (can appear multiple times)",
        "DIR",
    );
    opts.optflag("s", "single", "Accept only one client");
    opts.optopt("t", "terminate", "terminate after last client and no others for secs", "SECS");
    opts.optflag("v", "version", "show version info and exit");
    opts.optflag("w", "watchserver", "exit when server exits");
    opts
}

pub(crate) fn terminate_main_thread() {
    let main_thread = MAIN_THREAD.get().unwrap();
    assert!(
        *main_thread != pthread::pthread_self(),
        "terminate_main_thread called from main thread of initial process"
    );

    pthread::pthread_kill(*main_thread, Signal::SIGUSR1).unwrap();
}

// Make the conversion from raw fd to OnwedFd safer by checking that the fd is really present and
// open.  It still isn't a safe operation because it could be used to duplicate an OwnedFd
unsafe fn raw_to_owned(rawfd: RawFd) -> IoResult<OwnedFd> {
    #![allow(unsafe_code)]
    unsafe {
        rustix::fs::fstat(BorrowedFd::borrow_raw(rawfd))?;
        Ok(OwnedFd::from_raw_fd(rawfd))
    }
}
