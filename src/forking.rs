#![allow(unsafe_code)]

use nix::libc;
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::fork;
pub(crate) use nix::unistd::ForkResult;
use num_threads::is_single_threaded;

// Forks are only safe when the process is single-threaded.  Otherwise, a thread that isn't
// inherited by the child can hold a lock (or some equivalent state) that then won't be freeable in
// the child.  The num_threads::is_single_threaded predicate doesn't always work to determine this -
// there are platforms where it is known to not work well, and it requires `/proc` to be mounted
// properly.  However, it's better than nothing.
fn check_single_threaded() {
    assert!(
        is_single_threaded().expect("Could not determine single-threadedness prior to fork"),
        "The process is not single threaded prior to fork"
    );
}

// Use libc::_exit instead of std::process::exit so that no atexit handlers or signal handlers are
// called, and no buffers are flushed - in other words, don't interfere with the other forks.

pub(crate) unsafe fn double_fork() -> nix::Result<ForkResult> {
    check_single_threaded();
    match unsafe { fork() } {
        Ok(ForkResult::Child) => match unsafe { fork() } {
            c @ Ok(ForkResult::Child) => c,
            Ok(ForkResult::Parent { .. }) => unsafe { libc::_exit(0) },
            Err(_) => unsafe { libc::_exit(-1) },
        },
        p @ Ok(ForkResult::Parent { child, .. }) => {
            waitpid(Some(child), Some(WaitPidFlag::empty()))?;
            p
        }
        e @ Err(_) => e,
    }
}

pub(crate) unsafe fn daemonize() -> nix::Result<ForkResult> {
    check_single_threaded();
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => unsafe { libc::_exit(0) },
        r => r,
    }
}
