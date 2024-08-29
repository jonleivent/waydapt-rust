use crate::addons::IHMap;
use crate::basics::Leaker;
use crate::crate_traits::{Alloc, EventHandler};
use crate::for_handlers::SessionInitHandler;
use crate::handlers::{gather_handlers, SessionHandlers};
use crate::listener::SocketListener;
use crate::postparse::{active_interfaces, ActiveInterfaces};
use crate::session::client_session;
use getopts::{Matches, Options, ParsingStyle};
use nix::sys::pthread;
use nix::sys::signal;
use nix::sys::signalfd::{SfdFlags, SignalFd};
use rustix::event::epoll::EventFlags;
use rustix::fs::{flock, FlockOperation};
use signal::{SigSet, Signal};
use std::collections::VecDeque;
use std::env::Args;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct SharedOptions {
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
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
        print_version_info();
        return;
    }

    // gather options that can be used elsewhere:
    let options = SharedOptions::new(&matches);

    // do all protocol and globals parsing, and handler linkups:
    let (active_interfaces, session_handlers) =
        globals_and_handlers(&matches, all_args, init_handlers);

    // Start listening on the socket for our clients:
    let listener = start_listening(&matches);

    MAIN_THREAD.set(pthread::pthread_self()).unwrap();

    let mut socket_event_handler =
        SocketEventHandler::new(listener, options, active_interfaces, session_handlers);

    // Creating the SocketEventHandler establishes the signal mask and signalfd, allowing
    // termination signals from the following threads to be processed correctly:

    if matches.opt_present("w") {
        let server_lock_pathbuf = crate::listener::get_server_socket_path().with_extension("lock");
        let roptions = &options;
        std::thread::spawn(|| monitor_lock_file(server_lock_pathbuf, roptions));
    }

    if let Some(exit_lock_filename) = matches.opt_str("e") {
        let roptions = &options;
        std::thread::spawn(|| monitor_lock_file(exit_lock_filename, roptions));
    }

    match crate::event_loop::event_loop(&mut socket_event_handler) {
        Ok(()) => eprintln!("Normal termination"),
        Err(e) if e.kind() == ErrorKind::Interrupted => {
            if let Some(e) = e.into_inner() {
                eprintln!("Interrupted with {e}");
            } else {
                eprintln!("Interrupted");
            }
        }
        Err(e) => panic!("{e}"),
    }
}

fn print_version_info() {
    if let Some(val) = option_env!("CARGO_PKG_VERSION") {
        println!("WAYDAPT SEMVER:      {val}");
    }

    if let Some(val) = option_env!("VERGEN_BUILD_TIMESTAMP") {
        println!("BUILD TIMESTAMP:     {val}");
    }

    if let Some(val) = option_env!("VERGEN_CARGO_DEBUG") {
        println!("CARGO DEBUG:         {val}");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_DEPENDENCIES") {
        println!("CARGO DEPENDENCIES:  [{val}]");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_FEATURES") {
        println!("CARGO FEATURES:      [{val}]");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_OPT_LEVEL") {
        println!("CARGO OPT LEVEL:     {val}");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_TARGET_TRIPLE") {
        println!("CARGO TARGET TRIPLE: {val}");
    }

    if let Some(val) = option_env!("VERGEN_GIT_DESCRIBE") {
        println!("GIT DESCRIBE:        {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_BRANCH") {
        println!("GIT BRANCH:          {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_COMMIT_DATE") {
        println!("GIT COMMIT DATE:     {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_SHA") {
        println!("GIT SHA:             {val}");
    }

    if let Some(val) = option_env!("VERGEN_RUSTC_CHANNEL") {
        println!("RUSTC CHANNEL:       {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_COMMIT_DATE") {
        println!("RUSTC COMMIT DATE:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_COMMIT_HASH") {
        println!("RUSTC COMMIT HASH:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_HOST_TRIPLE") {
        println!("RUSTC HOST TRIPLE:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_LLVM_VERSION") {
        println!("RUSTC LLVM VERSION:  {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_SEMVER") {
        println!("RUSTC SEMVER:        {val}");
    }
}

impl SharedOptions {
    fn new(matches: &Matches) -> &'static Self {
        let wo = Leaker.alloc(SharedOptions {
            terminate_after: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
            flush_every_send: matches.opt_present("f"),
            debug_level: match std::env::var("WAYLAND_DEBUG").as_ref().map(String::as_str) {
                Ok("1") => 1,
                Ok("client") => 2,
                Ok("server") => 3,
                _ => 0,
            },
        });

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
        use std::{fs::File, io::BufWriter};
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
    opts.optopt("e", "exitlock", "file to monitor for exiting when unlocked", "FILE");
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
    opts.optopt("t", "terminate", "terminate after last client and no others for secs", "SECS");
    opts.optflag("v", "version", "Show version info and exit");
    opts.optflag("w", "watchserver", "Exit when server exits");
    opts
}

struct SocketEventHandler {
    listener: SocketListener,
    signalfd: SignalFd,
    options: &'static SharedOptions,
    interfaces: &'static ActiveInterfaces,
    handlers: &'static VecDeque<SessionInitHandler>,
}

impl SocketEventHandler {
    fn new(
        listener: SocketListener, options: &'static SharedOptions,
        interfaces: &'static ActiveInterfaces, handlers: &'static VecDeque<SessionInitHandler>,
    ) -> Self {
        let mut mask = SigSet::empty();
        mask.add(signal::SIGTERM);
        mask.add(signal::SIGHUP);
        mask.add(signal::SIGINT);
        mask.add(signal::SIGUSR1); // for terminate_main_thread
        mask.thread_block().unwrap();
        let signalfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK).unwrap();

        SocketEventHandler { listener, signalfd, options, interfaces, handlers }
    }

    fn spawn_session(&self, client_stream: UnixStream) {
        let options = self.options;
        let interfaces = self.interfaces;
        let handlers = self.handlers;
        let session = move || client_session(options, interfaces, handlers, client_stream);
        std::thread::spawn(session);
    }

    fn handle_signal_input(&mut self) -> IoResult<Option<()>> {
        #[allow(clippy::cast_possible_wrap)]
        let sig = match self.signalfd.read_signal() {
            Ok(Some(ref sig)) => sig.ssi_signo as i32,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let signal = Signal::try_from(sig).unwrap();
        let err = Error::new(ErrorKind::Interrupted, signal.as_ref());
        Err(err)
    }

    fn handle_listener_input(&mut self) -> IoResult<Option<()>> {
        let client_stream = match self.listener.accept() {
            Ok((client_stream, _)) => client_stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        self.spawn_session(client_stream);
        Ok(None)
    }
}

impl EventHandler for SocketEventHandler {
    type InputResult = ();

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)> {
        let flags = EventFlags::IN; // level triggered
        [(self.listener.as_fd(), flags), (self.signalfd.as_fd(), flags)].into_iter()
    }

    fn handle_input(&mut self, fd_index: usize) -> IoResult<Option<()>> {
        match fd_index {
            0 => self.handle_listener_input(),
            1 => self.handle_signal_input(),
            _ => unreachable!(),
        }
    }

    fn handle_output(&mut self, _fd_index: usize) -> IoResult<()> {
        unreachable!("We didn't ask for no output!")
    }
}

pub(crate) fn terminate_main_thread() {
    let main_thread = MAIN_THREAD.get().unwrap();
    assert!(
        *main_thread != pthread::pthread_self(),
        "terminate_main_thread called from main thread of initial process"
    );

    pthread::pthread_kill(*main_thread, Signal::SIGUSR1).unwrap();
}

#[allow(unused)]
fn monitor_lock_file(path: impl AsRef<Path>, options: &'static SharedOptions) {
    let pathname = path.as_ref().to_str().unwrap();
    let lock_file = std::fs::File::open(&path).unwrap_or_else(|e| {
        eprintln!("Cannot open {pathname} : {e:?}");
        std::process::exit(1);
    });
    flock(&lock_file, FlockOperation::LockShared).unwrap_or_else(|e| {
        if e == rustix::io::Errno::INTR {
            return;
        }
        eprintln!("Cannot flock {pathname} : {e:?}");
        std::process::exit(1);
    });
    eprintln!("Exiting due to unlocked {pathname}");
    terminate_main_thread();
}

// Make the conversion from raw fd to OnwedFd safer by checking that the fd is really present and
// open.  It still isn't a safe operation because it could be used to duplicate the OwnedFd on this
// fd.
unsafe fn raw_to_owned(rawfd: RawFd) -> IoResult<OwnedFd> {
    #![allow(unsafe_code)]
    unsafe {
        rustix::fs::fstat(BorrowedFd::borrow_raw(rawfd))?;
        Ok(OwnedFd::from_raw_fd(rawfd))
    }
}
