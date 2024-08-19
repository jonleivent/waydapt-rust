#![warn(clippy::pedantic)]
#![deny(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]

pub mod addons;
pub mod basics;
pub mod buffers;
pub mod builtin;
pub mod crate_traits;
pub mod event_loop;
pub mod for_handlers;
#[cfg(feature = "forking")]
pub mod forking;
pub mod handlers;
pub mod header;
pub mod listener;
pub mod map;
pub mod mediator;
pub mod message;
#[cfg(feature = "cleanup")]
pub mod multithread_exit;
pub mod parse;
pub mod postparse;
pub mod protocol;
pub mod session;
pub mod setup;
pub mod streams;
#[cfg(feature = "terminator")]
pub mod terminator;

fn main() -> std::process::ExitCode { setup::startup(&addons::get_addon_handlers()) }
