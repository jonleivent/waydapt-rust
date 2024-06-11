extern crate arrayvec;
extern crate getopts;
extern crate libc;
extern crate nix;
extern crate rustix;
extern crate static_assertions;

pub mod buffers;
//pub mod handler;
pub mod for_handlers;
pub mod map;
pub mod parse;
pub mod postparse;
pub mod protocol;
pub mod setup;

fn main() {
    println!("Hello, world!");
}
