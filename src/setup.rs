use crate::basics::Leaker;
use crate::crate_traits::Alloc;
use crate::for_handlers::InitHandlersFun;
use crate::handlers::{SessionHandlers, gather_handlers};
use crate::listener::SocketListener;
use crate::postparse::{ActiveInterfaces, active_interfaces};
use crate::session::client_session;
use crate::socket_events::SocketEventHandler;
use getopts::{Matches, Options, ParsingStyle};
use nix::sys::pthread;
use nix::sys::signal;
use rustix::fs::{FlockOperation, flock};
use signal::Signal;
use std::collections::HashMap;
use std::env::Args;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use std::{fs::File, io::BufWriter};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct SharedOptions {
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
    pub(crate) watch_server: bool,
    pub(crate) single_mode: bool,
    #[cfg(feature = "forking")]
    pub(crate) fork_mode: bool,
}

static MAIN_THREAD: OnceLock<pthread::Pthread> = OnceLock::new();

pub(crate) type IHMap = HashMap<&'static str, InitHandlersFun>;

fn get_addon_handlers() -> IHMap { crate::addons::ALL_ADDONS.iter().copied().collect() }

pub(crate) fn startup() {
    let init_handlers = get_addon_handlers();
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
    let (interfaces, handlers) = globals_and_handlers(&matches, all_args, &init_handlers);

    // Note - if using thread-spawning (no -c or -s options), the client session is started within
    // the event loop inside listen, and only errors are returned below:
    match listen(&matches, options, interfaces, handlers) {
        Ok(client_stream) => client_session(options, interfaces, handlers, client_stream), // -c|-s
        Err(e) => match (e.kind(), e.into_inner()) {
            (ErrorKind::Interrupted, Some(i)) => eprintln!("Interrupted with {i}"),
            (ErrorKind::Interrupted, None) => eprintln!("Interrupted"),
            (ErrorKind::Other, Some(i)) => eprintln!("Normal termination: {i}"),
            (ErrorKind::Other, None) => eprintln!("Normal termination"),
            (k, Some(i)) => panic!("{k}: {i}"),
            (k, None) => panic!("{k}"),
        },
    };
}

fn listen(
    matches: &Matches, options: &'static SharedOptions, interfaces: &'static ActiveInterfaces,
    handlers: SessionHandlers,
) -> IoResult<UnixStream> {
    // Group the listener and event handler here so that they are both dropped in the -c|-s cases
    // before we start the client_session:

    let display_name = &matches.opt_str("d").unwrap_or("waydapt-0".into()).into();
    let mut listener = SocketListener::new(display_name); // socket is ready after this

    // If we've been given an anti-lock fd, unlock it now to allow clients waiting on it to start:
    if let Some(anti_lock_fd) = matches.opt_str("a") {
        let raw = anti_lock_fd.parse().expect("-a fd : must be an int");
        assert!(raw >= 0, "-a {raw}: fd must not be negative");
        #[allow(unsafe_code)]
        unsafe {
            flock(BorrowedFd::borrow_raw(raw), FlockOperation::Unlock).unwrap_or_else(|e| {
                panic!("Anti-lock (-a) fd={raw} does not correspond to an open file or dir: {e}");
            });
            // should we bother trying to close it?  That is more unsafe, because the fd might be
            // manifested as an OnwedFd elsewhere that expects to stay open for its lifetime.
        }
    };

    // Daemonize after the socket is ready (another way clients can be notified of readiness):
    #[cfg(feature = "forking")]
    if matches.opt_present("z") {
        use crate::forking::daemonize;
        #[allow(unsafe_code)]
        unsafe { daemonize() }.unwrap_or_else(|e| panic!("Could not daemonize!: {e:?}"));

        listener.reset_init_pid();
    };

    // Keep global track of this main thread for signalling purposes - do this after any potential
    // daemonizing (above) since daemonizing can change the main thread's id.  Even though we don't
    // currently use MAIN_THREAD in the daeminizing case.
    MAIN_THREAD.set(pthread::pthread_self()).unwrap();

    let mut socket_event_handler = SocketEventHandler::new(listener, options, interfaces, handlers);

    // accept new clients - this only returns Ok with -c or -s options.  In other cases, it
    // spawns the client sessions internally as threads:
    crate::event_loop::event_loop(&mut socket_event_handler)
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
            #[cfg(feature = "forking")]
            fork_mode: matches.opt_present("c"),
        });
        assert!(
            !(so.terminate_after.is_some() && so.single_mode),
            "-t and -s are mutually exclusive"
        );
        #[cfg(feature = "forking")]
        {
            assert!(!(so.single_mode && so.fork_mode), "-c and -s are mutually exclusive");
            assert!(
                !(so.terminate_after.is_some() && so.fork_mode),
                "-t and -c are mutually exclusive"
            );
        }
        so
    }
}

// Construct a single iterator over all protocol files and protocol directories.
fn protocol_file_iter<'a>(
    files: &'a [String], dirs: &'a [String],
) -> impl Iterator<Item = PathBuf> + 'a {
    files.iter().map(PathBuf::from).chain(dirs.iter().flat_map(|d| {
        std::fs::read_dir(d).unwrap().filter_map(|f| {
            let f = f.unwrap().path();
            match f.extension() {
                // only files with xml extension in protocol dirs:
                Some(e) if e.eq_ignore_ascii_case("xml") => Some(f),
                _ => None,
            }
        })
    }))
}

fn globals_and_handlers(
    matches: &Matches, all_args: &mut Args, init_handlers: &IHMap,
) -> (&'static ActiveInterfaces, SessionHandlers) {
    let protocol_files = matches.opt_strs("p");
    let protocol_dirs = matches.opt_strs("P");
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

fn get_options() -> Options {
    let mut opts = Options::new();
    opts.parsing_style(ParsingStyle::StopAtFirstFree)
        .optopt(
            "a",
            "antilock",
            "file descriptor for waydapt to unlock when its socket is ready",
            "FILE-DESCRIPTOR",
        )
        .optopt("d", "display", "the name of the Wayland display socket to create", "NAME")
        .optflag("f", "flushsends", "send every message immediately, instead of batching them")
        .optopt(
            "g",
            "globals",
            "file with one allowed global interface and max version per line",
            "FILE",
        )
        .optflag("h", "help", "print this help")
        .optflag("l", "list", "list available addon modules")
        .optflag("m", "allowmissing", "allow global entries that don't appear in protocol files")
        .optopt("o", "output", "dump processed protocol and handler info to file", "FILE")
        .optmulti("p", "protofile", "a protocol XML file (can appear multiple times)", "FILE")
        .optmulti(
            "P",
            "protodir",
            "a directory of protocol XML files (can appear multiple times)",
            "DIR",
        )
        .optflag("s", "single", "Accept only one client")
        .optopt("t", "terminate", "terminate after last client and no others for secs", "SECS")
        .optflag("v", "version", "show version info and exit")
        .optflag("w", "watchserver", "exit when server exits");

    #[cfg(feature = "forking")]
    {
        opts.optflag("c", "childprocs", "Fork client sessions into child processes");
        opts.optflag("z", "daemonize", "Daemonize when client socket is ready");
    }
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
