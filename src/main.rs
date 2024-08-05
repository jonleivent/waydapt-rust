pub mod addons;
pub mod basics;
pub mod buffers;
pub mod builtin;
pub mod crate_traits;
pub mod event_loop;
pub mod for_handlers;
pub mod forking;
pub mod handlers;
pub mod header;
pub mod input_handler;
pub mod listener;
pub mod map;
pub mod message;
pub mod multithread_exit;
pub mod parse;
pub mod postparse;
pub mod protocol;
pub mod session;
pub mod setup;
pub mod streams;
pub mod terminator;

fn main() -> std::process::ExitCode {
    setup::startup(&addons::get_addon_handlers())
}
