#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

pub use crate::message::ArgData;
pub use crate::postparse::ActiveInterfaces;
pub use crate::protocol::{Interface, Message};

pub trait MessageInfo<'a> {
    fn get_num_args(&self) -> usize;
    fn get_arg(&self, index: usize) -> &ArgData<'a>;
    fn get_arg_mut(&mut self, index: usize) -> &mut ArgData<'a>;
    fn get_args_mut(&mut self) -> &mut [ArgData<'a>];
    fn get_decl(&self) -> &'static Message<'static>;
    fn get_object_id(&self) -> u32;
    fn set_arg(&mut self, index: usize, a: ArgData<'a>);
}

pub trait SessionInitInfo {
    fn ucred(&self) -> rustix::net::UCred;
    fn get_active_interfaces(&self) -> &'static ActiveInterfaces;
    // TBD: get method for active interfaces, etc.
    fn get_display(&self) -> RInterface; // wl_display
}

pub type RInterface = &'static Interface<'static>;

pub trait SessionInfo: SessionInitInfo {
    fn try_lookup(&self, id: u32) -> Option<RInterface>;
    fn lookup(&self, id: u32) -> RInterface;
    fn add(&mut self, id: u32, interface: RInterface);
    fn delete(&mut self, id: u32);
}

pub enum MessageHandlerResult {
    Next,
    Send,
    Drop,
}

// TBD: these fn types could be made more general.  They need to be Sendable.  thread::spawn uses:
// pub fn spawn<F, T>(f: F) -> JoinHandle<T>
// where
//    F: FnOnce() -> T + Send + 'static,
//    T: Send + 'static,
// We need something like this but with Fn instead of FnOnce.  Probably &dyn as well.

pub type MessageHandler = fn(&mut dyn MessageInfo, &mut dyn SessionInfo) -> MessageHandlerResult;

pub type SessionInitHandler = fn(&dyn SessionInitInfo);

// TBD: a more general callable type is Box<dyn FnMut(...) + Clone> that we clone into each session
// as needed.  But why does it need state?  The MessageHandlers can't have state.  If we want each
// addon to have per-session state, they need to use thread_local somehow to set that up.

// It would make sense if SessionInitHandler was FnOnce + Clone, and if MessageHandler was FnMut +
// Clone if we somehow arrange for each session to have its own copy.

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
