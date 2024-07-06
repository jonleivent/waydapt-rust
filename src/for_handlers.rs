#![warn(clippy::pedantic)]

pub use crate::message::ArgData;
pub use crate::postparse::ActiveInterfaces;
pub use crate::protocol::{Interface, Message};
use std::cell::Cell;

pub trait MessageInfo<'a> {
    fn get_num_args(&self) -> usize;
    fn get_arg(&self, index: usize) -> &ArgData<'a>;
    fn get_arg_mut(&mut self, index: usize) -> &mut ArgData<'a>;
    fn get_cell_args(&mut self) -> &[Cell<ArgData<'a>>];
    fn get_decl(&self) -> &'static Message<'static>;
    fn get_object_id(&self) -> u32;
    fn set_arg(&mut self, index: usize, a: ArgData<'a>);
}

pub trait SessionInitInfo {
    fn ucred(&self) -> rustix::net::UCred;
    fn get_active_interfaces(&self) -> &'static ActiveInterfaces;
    // TBD: get method for active interfaces, etc.
}

pub type RInterface = &'static Interface<'static>;

pub trait SessionInfo: SessionInitInfo {
    fn lookup(&self, id: u32) -> RInterface;
    fn add(&mut self, id: u32, interface: RInterface);
    fn delete(&mut self, id: u32);
}

pub enum MessageHandlerResult {
    Next,
    Send,
    Drop,
}

pub type MessageHandler = fn(&mut dyn MessageInfo, &mut dyn SessionInfo) -> MessageHandlerResult;

pub type SessionInitHandler = fn(&dyn SessionInitInfo);

pub trait AddHandler {
    fn request_push_front(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    );
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    );

    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    );
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    );

    fn session_push_front(&mut self, handler: SessionInitHandler);
    fn session_push_back(&mut self, handler: SessionInitHandler);
}

pub type InitHandlersFun = fn(&[String], &mut dyn AddHandler);
