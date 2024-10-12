#![forbid(unsafe_code)]

pub use crate::basics::MAX_BYTES_OUT;
pub use crate::message::ArgData;
pub use crate::postparse::ActiveInterfaces;
pub use crate::protocol::{Interface, Message, Type};

pub trait MessageInfo<'a> {
    fn get_num_args(&self) -> usize;
    fn get_arg(&self, index: usize) -> &ArgData<'a>;
    fn get_arg_mut(&mut self, index: usize) -> &mut ArgData<'a>;
    fn get_args_mut(&mut self) -> &mut [ArgData<'a>];
    fn get_decl(&self) -> &'static Message<'static>;
    fn get_object_id(&self) -> u32;
    fn set_arg(&mut self, index: usize, a: ArgData<'a>);
    fn get_size(&self) -> usize;
}

pub type RInterface = &'static Interface<'static>;

pub trait SessionInitInfo {
    fn ucred(&self) -> Option<rustix::net::UCred>;
    fn get_active_interfaces(&self) -> &'static ActiveInterfaces;
    fn get_display(&self) -> RInterface; // wl_display
    fn get_debug_level(&self) -> u32;
}

pub trait SessionInfo: SessionInitInfo {
    fn try_lookup(&self, id: u32) -> Option<RInterface>;
    fn lookup(&self, id: u32) -> RInterface;
    fn add(&mut self, id: u32, interface: RInterface);
    fn delete(&mut self, id: u32);
}

#[derive(PartialEq, Eq, Debug)]
pub enum MessageHandlerResult {
    Next,
    Send,
    Drop,
}

// The usize final args are for the addon group (starting from 0 on the command line)
pub type MessageHandler =
    fn(&mut dyn MessageInfo, &mut dyn SessionInfo, usize) -> MessageHandlerResult;

pub type SessionInitHandler = fn(&dyn SessionInitInfo, usize);

#[derive(Debug)]
pub enum AddHandlerError {
    NoSuchInterface,
    NoSuchRequest,
    NoSuchEvent,
    InactiveRequest,
    InactiveEvent,
}

pub trait AddHandler {
    #![allow(clippy::missing_errors_doc)]
    fn request_push_front(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError>;
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError>;

    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError>;
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError>;

    fn session_push_front(&mut self, handler: SessionInitHandler);
    fn session_push_back(&mut self, handler: SessionInitHandler);
}

pub type InitHandlersFun = fn(&[String], &mut dyn AddHandler, &'static ActiveInterfaces, usize);
