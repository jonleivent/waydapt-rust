extern crate arrayvec;
extern crate getopts;
extern crate libc;
extern crate nix;
extern crate rustix;
extern crate static_assertions;

pub mod buffers;
pub mod event_loop;
pub mod for_handlers;
pub mod forking;
pub mod input_handler;
pub mod listener;
pub mod map;
pub mod multithread_exit;
pub mod parse;
pub mod postparse;
pub mod protocol;
pub mod session;
pub mod setup;
pub mod streams;
pub mod terminator;

const MAX_FDS_OUT: usize = 28;
const MAX_BYTES_OUT: usize = 4096;

fn main() -> std::process::ExitCode {
    crate::setup::startup()
}
