#![warn(clippy::pedantic)]
#![allow(dead_code)]
#![forbid(unsafe_code)]

use crate::buffers::OutBuffer;
use crate::crate_traits::{ClientPeer, FdInput, Messenger, ServerPeer};
#[allow(clippy::enum_glob_use)]
use crate::for_handlers::{
    AddHandler, ArgData, MessageHandlerResult, MessageHandlerResult::*, MessageInfo, RInterface,
    SessionInfo, SessionInitInfo,
};
use crate::header::MessageHeader;
use crate::map::{WaylandObjectMap, WL_SERVER_ID_START};
use crate::message::DemarshalledMessage;
use crate::postparse::ActiveInterfaces;
use crate::protocol::Message;
use crate::session::WaydaptSessionInitInfo;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;
use std::sync::OnceLock;

////////////////////////////////////////////////////////////////////////////////

// TBD: where should this go?
impl FdInput for VecDeque<OwnedFd> {
    fn try_take_fd(&mut self) -> Option<OwnedFd> {
        self.pop_front()
    }

    fn drain(&mut self, num: usize) -> impl Iterator<Item = OwnedFd> {
        self.drain(..num)
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug)]
pub(crate) struct IdMap {
    pub(crate) client_id_map: WaylandObjectMap<ClientPeer>,
    pub(crate) server_id_map: WaylandObjectMap<ServerPeer>,
}

impl IdMap {
    pub(crate) fn new() -> Self {
        Self { client_id_map: WaylandObjectMap::new(), server_id_map: WaylandObjectMap::new() }
    }

    pub(crate) fn try_lookup(&self, id: u32) -> Option<RInterface> {
        if id >= WL_SERVER_ID_START {
            self.server_id_map.lookup(id)
        } else {
            self.client_id_map.lookup(id)
        }
    }

    pub(crate) fn lookup(&self, id: u32) -> RInterface {
        let i = self.try_lookup(id);
        i.unwrap_or_else(|| panic!("No interface for object id {id}"))
    }

    pub(crate) fn add(&mut self, id: u32, interface: RInterface) {
        if id >= WL_SERVER_ID_START {
            self.server_id_map.add(id, interface);
        } else {
            self.client_id_map.add(id, interface);
        };
    }

    pub(crate) fn delete(&mut self, id: u32) {
        if id >= WL_SERVER_ID_START {
            self.server_id_map.delete(id);
        } else {
            self.client_id_map.delete(id);
        };
    }
}

#[derive(Debug)]
pub(crate) struct Mediator<'a> {
    id_map: IdMap,
    init_info: &'a WaydaptSessionInitInfo,
}

impl<'a> SessionInitInfo for Mediator<'a> {
    fn ucred(&self) -> rustix::net::UCred {
        self.init_info.ucred
    }

    fn get_active_interfaces(&self) -> &'static ActiveInterfaces {
        self.init_info.active_interfaces
    }
}

impl<'a> SessionInfo for Mediator<'a> {
    fn try_lookup(&self, id: u32) -> Option<RInterface> {
        self.id_map.try_lookup(id)
    }

    fn lookup(&self, id: u32) -> RInterface {
        self.id_map.lookup(id)
    }

    fn add(&mut self, id: u32, interface: RInterface) {
        self.id_map.add(id, interface);
    }

    fn delete(&mut self, id: u32) {
        self.id_map.delete(id);
    }
}

impl<'a> Mediator<'a> {
    pub(crate) fn new(init_info: &'a WaydaptSessionInitInfo) -> Self {
        let mut s = Self { id_map: IdMap::new(), init_info };
        // the id map always has the wl_display interface at id 1:
        s.id_map.add(1, init_info.active_interfaces.get_display());
        s
    }

    pub(crate) fn mediate(
        &mut self, index: usize, in_msg: &[u32], in_fds: &mut impl FdInput, out: &mut OutBuffer,
    ) -> IoResult<()> {
        // The demarshalling and remarshalling, along with message handlers:
        let from_server = index > 0;
        let header = MessageHeader::new(in_msg);
        let interface = self.lookup(header.object_id);
        let msg_decl = interface.get_message(from_server, header.opcode as usize);
        if let Some(handlers) = msg_decl.handlers.get() {
            let mut dmsg = DemarshalledMessage::new(header, msg_decl, in_msg);
            dmsg.demarshal(in_fds, self);
            self.debug_in(header, msg_decl, &dmsg, from_server);
            for (_mod_name, h) in handlers {
                // since we have the mod name, we can debug each h call along with their result - TBD
                match h(&mut dmsg, self) {
                    Next => continue,
                    Send => break,
                    Drop => {
                        self.debug_drop(header, msg_decl, from_server);
                        return Ok(());
                    }
                }
            }
            self.debug_out(header, msg_decl, &dmsg, from_server);
            dmsg.marshal(out)?;
        } else if msg_decl.new_id_interface.get().is_some()
            || self.init_info.options.debug_level != 0
        {
            // Demarshal just to process new_id arg, or for debugging.  We could optimize this to
            // only do the work needed to eventually call id_map.add for the new_id, but in most
            // cases, a new_id bearing message will have very few other arguments (usually none), so
            // the wasted work is minimal.  Best to keep the same code path:
            let mut dmsg = DemarshalledMessage::new(header, msg_decl, in_msg);
            dmsg.demarshal(in_fds, self);
            self.debug_unified(header, msg_decl, &dmsg, from_server);
            dmsg.relay_unmodified(out)?;
        } else {
            // No demarshalling needed - this path allows transfering data directly from the input
            // buffer to the output buffer with no intermediate copies, and should be the choice for
            // most of the message traffic
            let num_fds = msg_decl.num_fds as usize;
            out.send_raw(in_fds.drain(num_fds), in_msg)?;
        }

        Ok(())
    }
}

mod debug {
    use super::{DemarshalledMessage, Mediator, Message, MessageHeader};

    impl<'a> Mediator<'a> {
        fn eprint_flow_unified(&self, from_server: bool) {
            if from_server {
                eprint!("server->waydapt->client[{}]", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("client[{}]->waydapt->server", self.init_info.ucred.pid.as_raw_nonzero());
            }
        }

        fn eprint_flow_in(&self, from_server: bool) {
            if from_server {
                eprint!("server->waydapt[{}]", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("client[{}]->waydapt", self.init_info.ucred.pid.as_raw_nonzero());
            }
        }

        fn eprint_flow_out(&self, from_server: bool) {
            if from_server {
                eprint!("waydapt->client[{}]", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("waydapt[{}]->server", self.init_info.ucred.pid.as_raw_nonzero());
            }
        }

        pub(super) fn debug_in(
            &self, header: MessageHeader, msg_decl: &Message, dmsg: &DemarshalledMessage,
            from_server: bool,
        ) {
            if match self.init_info.options.debug_level {
                1 => true,
                2 => !from_server,
                3 => from_server,
                _ => false,
            } {
                debug_print(
                    header,
                    msg_decl,
                    || self.eprint_flow_in(from_server),
                    || dmsg.debug_print(self),
                );
            }
        }

        pub(super) fn debug_out(
            &self, header: MessageHeader, msg_decl: &Message, dmsg: &DemarshalledMessage,
            from_server: bool,
        ) {
            if match self.init_info.options.debug_level {
                1 => true,
                2 => from_server,
                3 => !from_server,
                _ => false,
            } {
                debug_print(
                    header,
                    msg_decl,
                    || self.eprint_flow_out(from_server),
                    || dmsg.debug_print(self),
                );
            }
        }

        #[inline]
        pub(super) fn debug_unified(
            &self, header: MessageHeader, msg_decl: &Message, dmsg: &DemarshalledMessage,
            from_server: bool,
        ) {
            if self.init_info.options.debug_level != 0 {
                debug_print(
                    header,
                    msg_decl,
                    || self.eprint_flow_unified(from_server),
                    || dmsg.debug_print(self),
                );
            }
        }

        pub(super) fn debug_drop(
            &self, header: MessageHeader, msg_decl: &Message, from_server: bool,
        ) {
            if self.init_info.options.debug_level != 0 {
                debug_print(
                    header,
                    msg_decl,
                    || self.eprint_flow_out(from_server),
                    || eprint!("dropped!"),
                );
            }
        }
    }

    fn debug_print<F1, F2>(header: MessageHeader, msg_decl: &Message, flow: F1, body: F2)
    where
        F1: FnOnce(),
        F2: FnOnce(),
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let microsecs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        let interface_name = &msg_decl.owner.get().unwrap().name;
        let message_name = &msg_decl.name;
        let target_id = header.object_id;
        eprint!("[{:7}.{:03}] ", microsecs / 1000, microsecs % 1000);
        flow();
        eprint!("{interface_name}#{target_id}.{message_name}(");
        body();
        eprintln!(")");
    }
}

////////////////////////////////////////////////////////////////////////////////

fn wl_registry_global_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // arg0 is the "name" of the global instance, really a uint
    let ArgData::String(interface_name) = msg.get_arg(1) else { panic!() };
    let interface_name = interface_name.to_str().unwrap();
    let ArgData::Uint(advertised_version) = msg.get_arg(2) else { panic!() };
    let active_interfaces = session_info.get_active_interfaces();
    let Some(global_interface) = active_interfaces.maybe_get_global(interface_name) else {
        return MessageHandlerResult::Drop;
    };
    let limited_version = *global_interface.version().unwrap();
    // Do we use limited_version == 0 like the C version does for denoting an interface that was not
    // in the globals file?  Regardless, 0 means drop:
    if limited_version == 0 {
        return MessageHandlerResult::Drop;
    };
    if *advertised_version > limited_version {
        msg.set_arg(2, ArgData::Uint(limited_version));
    }
    MessageHandlerResult::Send
}

fn wl_display_delete_id_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    let ArgData::Object(id) = msg.get_arg(0) else { panic!() };
    // should we check if this is the wayland-idfix handshake initial message from the server?  If
    // it is, it has !0 as a server object id, which will never exist, so delete of it will do
    // nothing (it won't panic either).
    session_info.delete(*id);
    MessageHandlerResult::Send
}

fn wl_registry_bind_handler(
    _msg: &mut dyn MessageInfo, _session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // everything got handled in the add_wl_registry_bind_new_id method.  The only reason this
    // exists is to have a handler to force the handler path of input_handler::handle.
    MessageHandlerResult::Send
}

pub(crate) fn add_builtin_handlers(adder: &mut dyn AddHandler) {
    adder.event_push_front("wl_registry", "global", wl_registry_global_handler);
    adder.event_push_front("wl_display", "delete_id", wl_display_delete_id_handler);
    adder.request_push_front("wl_registry", "bind", wl_registry_bind_handler);
}

////////////////////////////////////////////////////////////////////////////////

mod safeclip {
    use std::ffi::CString;

    use super::{
        AddHandler, ArgData, Cow, MessageHandlerResult, MessageInfo, OnceLock, SessionInfo,
    };

    static PREFIX: OnceLock<CString> = OnceLock::new();

    fn init_handler(args: &[String], adder: &mut dyn AddHandler) {
        // we expect exactly one arg, which is the prefix.  Unlike the C waydapt, the 0th arg is NOT
        // the dll name, it is our first arg.
        assert_eq!(args.len(), 1);
        let prefix = &args[0];
        let cprefix = CString::new(prefix.clone()).unwrap();
        PREFIX.set(cprefix).unwrap();
        let requests = [
            ("wl_shell_surface", "set_title"),
            ("xdg_toplevel", "set_title"),
            ("wl_data_offer", "accept"),
            ("wl_data_offer", "receive"),
            ("wl_data_source", "offer"),
            ("zwp_primary_selection_offer_v1", "receive"),
            ("zwp_primary_selection_source_v1", "offer"),
            ("zwlr_data_control_source_v1", "offer"),
            ("zwlr_data_control_offer_v1", "receive"),
        ];
        for (iname, mname) in requests {
            adder.request_push_front(iname, mname, add_prefix);
        }
        let events = [
            ("wl_data_offer", "offer"),
            ("wl_data_source", "target"),
            ("wl_data_source", "send"),
            ("zwp_primary_selection_offer_v1", "offer"),
            ("zwp_primary_selection_source_v1", "send"),
            ("zwlr_data_control_source_v1", "send"),
            ("zwlr_data_control_offer_v1", "offer"),
        ];
        for (iname, mname) in events {
            adder.event_push_front(iname, mname, remove_prefix);
        }
        // We do not need a session init handler, because there is no per-session state
    }

    // Ideally, we could have a per-thread (per-session) buffer for concatenating where the prefix
    // is already present, and which gets borrowed by the Cow in the ArgData, so that there are no
    // allocations or frees.  A very elaborate alternative would be to have an additional field with
    // the Cow which is a Option closure that, if present, is called on the Cow and the output
    // buffer, and writes to the output buffer.  Actually, not a closure, but an object with at
    // least two methods (trait?): len and output.  The object should be stateless - may be zero
    // sized.  Box<dyn T> is fine for that, right?  Also, the output method would have to be very
    // low-level, like OutBuffer::push_array in terms of directing output to chunks, which would be
    // very difficult.  An alternative is to switch to 2*4K output chunks, so that no message needs
    // to be split over chunks, so can have simple output routines.  Or, alter push_arrray so that
    // it is based on a lower level push_slice, which does not write the length.  We would then also
    // need a push_zero_term.  The object, instead of being a trait, can be a static struct that has
    // two closure or funpointer fields.
    fn add_prefix(msg: &mut dyn MessageInfo, _si: &mut dyn SessionInfo) -> MessageHandlerResult {
        // find the first String arg and add PREFIX to the front of it:
        for i in 0..msg.get_num_args() {
            if let ArgData::String(s) = msg.get_arg(i) {
                let prefix = PREFIX.get().unwrap();
                let both = [prefix.to_bytes(), s.to_bytes()].concat();
                let cstring = CString::new(both).unwrap();
                msg.set_arg(i, ArgData::String(cstring.into()));
                return MessageHandlerResult::Next;
            }
        }
        panic!("Expected a string arg, didn't find one");
    }

    fn prefixed(prefix: &[u8], s: &[u8]) -> bool {
        let plen = prefix.len();
        s.len() >= plen && prefix == &s[..plen]
    }

    fn remove_prefix(msg: &mut dyn MessageInfo, _si: &mut dyn SessionInfo) -> MessageHandlerResult {
        // find the first String arg and remove PREFIX from the front of it:
        let prefix = PREFIX.get().unwrap().to_bytes();
        let plen = prefix.len();
        for i in 0..msg.get_num_args() {
            match msg.get_arg_mut(i) {
                // Almost all cases will be borrowed, which saves us a copy:
                ArgData::String(Cow::Borrowed(s)) if prefixed(prefix, s.to_bytes()) => {
                    *s = &s[plen..]; // O(1) move, no copy
                    return MessageHandlerResult::Next;
                }
                // Not worth optimizing this case - it would only happen if we have a previous handler
                // modifying this same string arg:
                ArgData::String(Cow::Owned(s)) if prefixed(prefix, s.to_bytes()) => {
                    *s = CString::new(&s.to_bytes()[plen..]).unwrap(); // O(N) copy
                    return MessageHandlerResult::Next;
                }
                ArgData::String(_) => return MessageHandlerResult::Drop,
                _ => {}
            }
        }
        panic!("Expected a string arg, didn't find one");
    }
}
