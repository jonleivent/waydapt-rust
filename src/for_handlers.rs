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

// TBD - it may be the case that we need a &mut dyn SessionInfo in order to add ids in the case of
// wl_registry::bind, unless we build that in.
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

// We want a MessageHandler to be able to:
// exit with Ok (drops message) or an Err (causes session shutdown)
// call the next handler
// send message
//
// both call next handler and send message are within scope of current handler, so that lifetimes of
// updates to message work
//
// Note that because the handler interfaces to both the demarsh message and the session through
// traits, that determines where a method to send has to be - it has to be on the message because
// that method has to be able to see into the demarsh message beyond just what is exposed by the
// trait, but as long as it has access to the OutChannel, that's all it needs from the session.
//
// If we don't care about possibly originating other messages, we could move the OutChannels to the
// DemarshalledMessage, so it has all it needs to send itself.  But that doesn't help with calling
// the next handler, which needs the SessionInfo as well.  So keep the OutChannels on the SessionInfo.

// Another possibility is that the demarsh message struct is the interface that the handler sees,
// not a trait.  We can control what it can use by pub control.  That would meaning moving the
// InputHander impl for WaydaptInputHandler into message.rs, so it can use the nonpub methods, like
// new.  But the problem with this is that the demarsh message has to be mut, and we don't want the
// handler to be able to modify anything except the individual args.  It should not for instance be
// able to push or pop args from the arg vec.  But we could make ALL of those fields private and
// just provide a few public methods.
