#![forbid(unsafe_code)]

/// What are Groups, AddOns, Sessions, Handlers?
///
/// A handler is a user function that is called for a particular occurence: either the reception of
/// a Wayland message (see MessageHandler), or the creation of a new client session (see
/// SessionInitHandler).
///
/// A session encapsulates all communication and state between a particular Wayland client process,
/// the waydapt process, and the compositor.  Each session is independent, and can run as a thread
/// or a subprocess of the waydapt process.
///
/// An addon is a set of handlers created by the user for some purpose (safeclip is an example).
///
/// A group is a section of the waydapt command line consisting of consecutive args destined for a
/// single instance of an addon.  There can be multiple groups on the waydapt command line, and even
/// multiple groups for the same addon.  Group arguments are delimited from other groups by the '--'
/// pseudo-arg.
///
/// A session consists of some state for each addon group.  In other words, if the safeclip addon is
/// mentioned twice on the waydapt command line, then each session will have two distinct safeclip
/// SessionState instances, one for each parameterization of safeclip from the command line.
///
/// Note that we often talk about the session state per addon, but this is really the session state
/// per group per addon.  It's just that, typically, there is only one group per addon.
///
/// Each addon is responsible for definining:
/// - SessionState type (can be ())
///     * a separate instance will exist per group per addon per session
///     * for holding user state of that addion group session
/// - InitHandlersFun
///     * called once per addon group
///     * is passed args for the addon group, along with an AddHandler for
///       registration of handlers for the addon group, and an ActiveInterfaces
///       for info about the protocol
///     * returns the SessionInitHandler for the addon group
///     * called only once per waydapt process per addon per group during
///       waydapt startup (after waydapt parses the protocol files)
/// - SessionInitHandler impl or fun/closure
///     * one per addon per group
///     * creates a SessionState instance
/// - multiple MessageHandler impls
///
/// It is required (but not checked) that the SessionState type per addon match across all of the
/// above.
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

// Each addon is responsible for:
// SessionState type (can be ())
// InitHandlersFun
// SessionInitHandler impl (init) - can be Fn
// multiple MessageHandler impls (handle) - can be Fns
// - must make sure that SessionState type matches across all above

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

// Null state case:
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
