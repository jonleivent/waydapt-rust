#![allow(unsafe_code)]

use nix::libc;
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::fork;
pub(crate) use nix::unistd::ForkResult;
use num_threads::is_single_threaded;

// Use libc::_exit instead of std::process::exit so that no atexit handlers or signal handlers are
// called, and no buffers are flushed - in other words, don't interfere with the other forks.

pub(crate) unsafe fn double_fork() -> nix::Result<ForkResult> {
    debug_assert!(is_single_threaded().unwrap());
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
    debug_assert!(is_single_threaded().unwrap());
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => libc::_exit(0),
        r => r,
    }
}
