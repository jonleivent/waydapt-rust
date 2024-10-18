#![forbid(unsafe_code)]

use std::any::Any;

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

// There is one InitHandlersFun per addon, and it is called once per group for that addon:
pub type InitHandlersFun = fn(
    &[String],
    &mut dyn AddHandler,
    &'static ActiveInterfaces,
) -> &'static dyn SessionInitHandler;

pub type SessionState = dyn Any;

pub trait SessionInitHandler: Sync {
    fn init(&self, session_init_info: &dyn SessionInitInfo) -> Box<SessionState>;
}

impl SessionInitHandler for () {
    fn init(&self, _: &dyn SessionInitInfo) -> Box<SessionState> { Box::new(()) }
}

// If you want to implement SessionInitHandler as a fun/closure instead:
impl<T: Sync> SessionInitHandler for T
where T: Fn(&dyn SessionInitInfo) -> Box<SessionState>
{
    fn init(&self, session_init_info: &dyn SessionInitInfo) -> Box<SessionState> {
        self(session_init_info)
    }
}

pub trait MessageHandler: Sync {
    fn handle(
        &self, mi: &mut dyn MessageInfo, si: &mut dyn SessionInfo, ss: &mut SessionState,
    ) -> MessageHandlerResult;
}

// If you want to implement MessageHandler as a fun/closure instead:
impl<T: Sync> MessageHandler for T
where T: Fn(&mut dyn MessageInfo, &mut dyn SessionInfo, &mut SessionState) -> MessageHandlerResult
{
    fn handle(
        &self, mi: &mut dyn MessageInfo, si: &mut dyn SessionInfo, ss: &mut SessionState,
    ) -> MessageHandlerResult {
        self(mi, si, ss)
    }
}

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
        handler: &'static dyn MessageHandler,
    ) -> Result<(), AddHandlerError>;
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: &'static dyn MessageHandler,
    ) -> Result<(), AddHandlerError>;

    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str,
        handler: &'static dyn MessageHandler,
    ) -> Result<(), AddHandlerError>;
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str,
        handler: &'static dyn MessageHandler,
    ) -> Result<(), AddHandlerError>;
}
