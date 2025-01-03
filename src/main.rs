#![warn(clippy::pedantic)]
#![deny(unsafe_code)]
#![forbid(clippy::large_types_passed_by_value)]
#![forbid(clippy::large_stack_frames)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

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
pub mod parse;
pub mod postparse;
pub mod protocol;
pub mod session;
pub mod setup;
pub mod socket_events;
pub mod streams;
pub mod terminator;
#[cfg(test)]
pub mod test_utils;
pub mod version_info;

fn main() { setup::startup() }
