use crate::addons::IHMap;
use crate::basics::Leaker;
use crate::crate_traits::{Alloc, EventHandler};
use crate::for_handlers::SessionInitHandler;
#[cfg(feature = "forking")]
use crate::forking::{daemonize, double_fork, ForkResult};
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
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct SharedOptions {
    #[cfg(feature = "forking")]
    pub(crate) fork_sessions: bool,
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
    #[cfg(feature = "forking")]
    pub(crate) setsid: bool,
}

static MAIN_ID: OnceLock<(/*pid*/ u32, pthread::Pthread)> = OnceLock::new();

pub(crate) fn startup(init_handlers: &IHMap) {
    static W_THREAD: OnceLock<pthread::Pthread> = OnceLock::new();
    static E_THREAD: OnceLock<pthread::Pthread> = OnceLock::new();

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

    let client_stream = {
        // Start listening on the socket for our clients:
        let listener = start_listening(&matches);

        // start_listening might daemonize, so don't set MAIN_ID until afterwards, but before we spawn
        // any threads:
        MAIN_ID.set((std::process::id(), pthread::pthread_self())).unwrap();

        let mut socket_event_handler =
            SocketEventHandler::new(listener, options, active_interfaces, session_handlers);

        // Creating the SocketEventHandler establishes the signal mask and signalfd, allowing
        // termination signals from the following threads to be processed correctly:

        if matches.opt_present("w") {
            let server_lock_pathbuf =
                crate::listener::get_server_socket_path().with_extension("lock");
            let roptions = &options;
            std::thread::spawn(|| monitor_lock_file(server_lock_pathbuf, roptions, &W_THREAD));
        }

        if let Some(exit_lock_filename) = matches.opt_str("e") {
            let roptions = &options;
            std::thread::spawn(|| monitor_lock_file(exit_lock_filename, roptions, &E_THREAD));
        }

        match crate::event_loop::event_loop(&mut socket_event_handler) {
            Ok(client_stream) => client_stream,
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                if let Some(e) = e.into_inner() {
                    eprintln!("Interrupted with {e:?}");
                } else {
                    eprintln!("Interrupted");
                }
                return;
            }
            e => panic!("{e:?}"),
        }
        // the above event loop only returns when we're in a forked child, and that causes this
        // scope to end and the SocketEventHandler to get dropped, which closes its associated fds
    };

    // In the forked child.  We have already removed most of what it doesn't need, except for the
    // two monitor_lock_file threads - which we will now (try to) kill off:

    // A race is remotely possible in each of these 2 cases between this thread and the target
    // thread setting the OnceLock.  But, not killing the threads only results in a waste of
    // resources in the forked clients in which they appear.  The monitor_lock_file is a no-op
    // (other than the resource waste) in processes other than the initial one.
    if let Some(e_thread) = E_THREAD.get() {
        let _ = pthread::pthread_kill(*e_thread, Signal::SIGUSR1);
    }
    if let Some(w_thread) = W_THREAD.get() {
        let _ = pthread::pthread_kill(*w_thread, Signal::SIGUSR1);
    }

    client_session(options, active_interfaces, session_handlers, client_stream);
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
            #[cfg(feature = "forking")]
            fork_sessions: matches.opt_present("c"),
            terminate_after: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
            flush_every_send: matches.opt_present("f"),
            #[cfg(feature = "forking")]
            setsid: matches.opt_present("s"),
            debug_level: match std::env::var("WAYLAND_DEBUG").as_ref().map(String::as_str) {
                Ok("1") => 1,
                Ok("client") => 2,
                Ok("server") => 3,
                _ => 0,
            },
        });

        #[cfg(feature = "forking")]
        assert!(
            !(wo.terminate_after.is_some() && wo.fork_sessions),
            "-t and -c cannot be used together"
        );
        #[cfg(feature = "forking")]
        if wo.setsid {
            assert!(wo.fork_sessions, "-s requres -c");
        }
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

    // Now that the socket is ready, we can use either or both of two ways to notify/allow clients
    // to request sessions: unlock the anti-lock fd and daemonizing this process.  Daemonizing is
    // the easiest, but it requires that the startup scripts be set up with clients run right after
    // the waydapt in the same script.  The anti-lock fd can be used to coordinate startup in more
    // complex environments, such as when the waydapt starts in its own sandbox (where daemonizing
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
    // for this parent process to exit:
    #[cfg(feature = "forking")]
    if matches.opt_present("z") {
        unsafe { daemonize() }.unwrap();
        rustix::process::setsid().unwrap();
    }

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
    #[cfg(feature = "forking")]
    opts.optflag("c", "childprocs", "make client sessions child processes instead of threads");
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
    opts.optflag("s", "setsid", "with -c, call setsid in each child process");
    opts.optopt(
        "t",
        "terminate",
        #[cfg(not(feature = "forking"))]
        "terminate after last client and no others for secs",
        #[cfg(feature = "forking")]
        "terminate after last client and no others for secs (can't be used with -c)",
        "SECS",
    );
    opts.optflag("v", "version", "Show version info and exit");
    opts.optflag("w", "watchserver", "Exit when server exits");
    #[cfg(feature = "forking")]
    opts.optflag("z", "daemonize", "daemonize waydapt when its socket is ready");
    opts
}

struct SocketEventHandler {
    listener: SocketListener,
    signalfd: SignalFd,
    options: &'static SharedOptions,
    interfaces: &'static ActiveInterfaces,
    handlers: &'static VecDeque<SessionInitHandler>,
}

#[allow(unused)]
impl SocketEventHandler {
    fn new(
        listener: SocketListener, options: &'static SharedOptions,
        interfaces: &'static ActiveInterfaces, handlers: &'static VecDeque<SessionInitHandler>,
    ) -> Self {
        let mut mask = SigSet::empty();
        mask.add(signal::SIGTERM);
        mask.add(signal::SIGHUP);
        mask.add(signal::SIGINT);
        mask.add(signal::SIGUSR1); // for self termination from sessions or monitor threads
        mask.thread_block().unwrap();
        let signalfd = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK).unwrap();

        SocketEventHandler { listener, signalfd, options, interfaces, handlers }
    }

    fn get_session(&self, client_stream: UnixStream) -> impl FnOnce() + Send + 'static {
        let options = self.options;
        let interfaces = self.interfaces;
        let handlers = self.handlers;
        move || client_session(options, interfaces, handlers, client_stream)
    }

    fn handle_signal_input(&mut self) -> IoResult<Option<UnixStream>> {
        // obviously, this never returns a UnixStream
        let sig = match self.signalfd.read_signal() {
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(Some(sig)) => sig,
        };
        #[allow(clippy::cast_possible_wrap)]
        let signal = Signal::try_from(sig.ssi_signo as i32).unwrap();
        let err = Error::new(ErrorKind::Interrupted, signal.as_ref());
        Err(err)
    }

    fn in_child_after_fork(&mut self) {
        self.listener.prevent_removes_on_drop();
        SigSet::all().thread_unblock().unwrap();
    }

    fn handle_listener_input(&mut self) -> IoResult<Option<UnixStream>> {
        match self.listener.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Ok((client_stream, _)) => {
                #[cfg(feature = "forking")]
                if self.options.fork_sessions {
                    #[allow(unsafe_code)]
                    if let ForkResult::Child = unsafe { double_fork() }.unwrap() {
                        self.in_child_after_fork();
                        if self.options.setsid {
                            rustix::process::setsid();
                        }
                        return Ok(Some(client_stream));
                    }
                    return Ok(None);
                }
                let session = self.get_session(client_stream);
                // dropping the returned JoinHandle detaches the thread, which is what we want
                std::thread::spawn(session);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

impl EventHandler for SocketEventHandler {
    type InputResult = UnixStream;

    fn fds_to_monitor(&self) -> impl Iterator<Item = (BorrowedFd<'_>, EventFlags)> {
        let flags = EventFlags::IN; // level triggered
        [(self.listener.as_fd(), flags), (self.signalfd.as_fd(), flags)].into_iter()
    }

    fn handle_input(&mut self, fd_index: usize) -> IoResult<Option<UnixStream>> {
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
    // The signalfd is only in the initial process.  Forked children must exit instead:
    let (main_pid, main_thread) = MAIN_ID.get().unwrap();
    if std::process::id() != *main_pid {
        std::process::exit(-1);
    }
    assert!(
        *main_thread != pthread::pthread_self(),
        "terminate_main_thread called from main thread of initial process"
    );

    pthread::pthread_kill(*main_thread, Signal::SIGUSR1).unwrap();
}

#[allow(unused)]
fn monitor_lock_file(
    path: impl AsRef<Path>, options: &'static SharedOptions, thread: &OnceLock<pthread::Pthread>,
) {
    thread.set(pthread::pthread_self()).unwrap();
    let pathname = path.as_ref().to_str().unwrap();
    let lock_file = std::fs::File::open(&path).unwrap_or_else(|e| {
        eprintln!("Cannot open {pathname} : {e:?}");
        std::process::exit(1);
    });

    flock(&lock_file, FlockOperation::LockShared).unwrap_or_else(|e| {
        if e == rustix::io::Errno::INTR {
            // this happens in forked child sessions, where we don't want to monitor lock files
            return;
        }
        eprintln!("Cannot flock {pathname} : {e:?}");
        std::process::exit(1);
    });
    let (main_pid, _) = MAIN_ID.get().unwrap();
    if std::process::id() != *main_pid {
        return;
    }
    eprintln!("Exiting due to unlocked {pathname}");
    terminate_main_thread();
}
