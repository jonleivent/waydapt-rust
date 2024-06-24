#![warn(clippy::pedantic)]

use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::fork;
pub(crate) use nix::unistd::ForkResult;

// Use libc::_exit instead of std::process::exit so that no destructors run, even when called from
// the main thread.

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
