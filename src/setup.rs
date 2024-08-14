use crate::addons::IHMap;
use crate::basics::Leaker;
use crate::crate_traits::Alloc;
use crate::for_handlers::SessionInitHandler;
use crate::forking::{daemonize, double_fork, ForkResult};
use crate::handlers::{gather_handlers, SessionHandlers};
use crate::listener::SocketListener;
use crate::multithread_exit::{multithread_exit, multithread_exit_handler, ExitCode};
use crate::postparse::{active_interfaces, ActiveInterfaces};
use crate::session::client_session;
use getopts::{Matches, Options, ParsingStyle};
use std::collections::VecDeque;
use std::env::Args;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::Duration;

use shadow_rs::shadow;

shadow!(build);

#[derive(Debug)]
pub(crate) struct SharedOptions {
    pub(crate) fork_sessions: bool,
    pub(crate) terminate_after: Option<Duration>,
    pub(crate) debug_level: u32,
    pub(crate) flush_every_send: bool,
}

pub(crate) fn startup(init_handlers: &IHMap) -> ExitCode {
    let all_args = &mut std::env::args();
    let program = all_args.next().unwrap();
    let our_args = all_args.take_while(|a| a != "--");
    let opts = get_options();
    let matches = opts.parse(our_args).unwrap();

    if matches.opt_present("h") {
        let brief = format!("Usage: {program} [options] -- ...dlls-and-their-options...");
        eprint!("{}", opts.usage(&brief));
        return ExitCode::SUCCESS;
    }

    if matches.opt_present("v") {
        eprintln!("{program} {}\n{}", build::VERSION, build::GIT_STATUS_FILE);
        return ExitCode::SUCCESS;
    }

    // gather options that can be used elsewhere:
    let options = SharedOptions::new(&matches);

    // do all protocol and globals parsing, and handler linkups:
    let (active_interfaces, session_handlers) =
        globals_and_handlers(&matches, all_args, init_handlers);

    // Start listening on the socket for our clients:
    let listener = start_listening(&matches);

    // How we accept new clients will be similar whether we fork sessions or not:
    let accept = || accept_clients(listener, options, active_interfaces, session_handlers);

    // If we're going to fork client sessions, then we don't need any special multi-threaded
    // exit magic (to enable other threads to end the process in an orderly fashion, with
    // destructors).  Otherwise, we do:
    if options.fork_sessions {
        accept(); // accept_clients in this thread
        ExitCode::SUCCESS
    } else {
        // accept clients in second thread, so that this main thread can wait: TBD: the second
        // thread has ownership of the listener, and so is responsible for removing the socket and
        // lock files - but it won't be told about the exit.  How to get it to shutdown properly and
        // do the listener drop?
        std::thread::spawn(accept);
        // Wait for any thread to report a problem, or exit due to the terminate option:
        multithread_exit_handler()
    }
}

impl SharedOptions {
    fn new(matches: &Matches) -> &'static Self {
        let wo = Leaker.alloc(SharedOptions {
            fork_sessions: matches.opt_present("c"),
            terminate_after: matches.opt_str("t").map(|t| Duration::from_secs(t.parse().unwrap())),
            flush_every_send: matches.opt_present("f"),
            debug_level: match std::env::var("WAYLAND_DEBUG").as_ref().map(String::as_str) {
                Ok("1") => 1,
                Ok("client") => 2,
                Ok("server") => 3,
                _ => 0,
            },
        });

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
    use rustix::fs::{flock, FlockOperation};

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
    opts.optflag("c", "childprocs", "make client sessions child processes instead of threads");
    opts.optopt("d", "display", "the name of the Wayland display socket to create", "NAME");
    opts.optflag("f", "flushsends", "send every message immediately, instead of batching them");
    opts.optopt(
        "g",
        "globals",
        "file with one allowed global interface and max version per line",
        "FILE",
    );
    opts.optflag("h", "help", "print this help");
    opts.optflag("m", "allowmissing", "allow global entries that don't appear in protocol files");
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
    opts.optflag("v", "version", "Show version info and exit");
    opts.optflag("z", "daemonize", "daemonize waydapt when its socket is ready");
    opts
}

fn accept_clients(
    listener: SocketListener, options: &'static SharedOptions,
    interfaces: &'static ActiveInterfaces, handlers: &'static VecDeque<SessionInitHandler>,
) {
    #![allow(unsafe_code)]
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
        let session = move || client_session(options, interfaces, handlers, client_stream);

        if fork_sessions {
            // we are in the main thread (because fork_sessions), so unwrap failing will terminate
            // the process:
            let ForkResult::Child = unsafe { double_fork() }.unwrap() else { continue };
            listener.drop_without_removes();
            return session();
        }
        // dropping the returned JoinHandle detaches the thread, which is what we want
        std::thread::spawn(session);
    }
}
