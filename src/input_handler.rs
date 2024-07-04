#![warn(clippy::pedantic)]
#![allow(dead_code)]
#![forbid(unsafe_code)]

use crate::basics::load_slice;
use crate::buffers::OutBuffer;
use crate::crate_traits::{FdInput, MessageSender, Messenger};
use crate::for_handlers::{
    AddHandler, ArgData, MessageHandlerResult, MessageInfo, RInterface, SessionInfo,
    SessionInitInfo,
};
use crate::map::{WaylandObjectMap, WL_SERVER_ID_START};
use crate::message::{DemarshalledMessage, MessageHeader};
use crate::postparse::ActiveInterfaces;
use crate::session::WaydaptSessionInitInfo;
use crate::streams::IOStream;
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
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) struct IdMap {
    pub(crate) client_id_map: WaylandObjectMap<false>,
    pub(crate) server_id_map: WaylandObjectMap<true>,
}

impl IdMap {
    pub(crate) fn new() -> Self {
        Self { client_id_map: WaylandObjectMap::new(), server_id_map: WaylandObjectMap::new() }
    }

    pub(crate) fn lookup(&self, id: u32) -> RInterface {
        let i = if id >= WL_SERVER_ID_START {
            self.server_id_map.lookup(id)
        } else {
            self.client_id_map.lookup(id)
        };
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

// This should hold all marshal/demarshal relevant info, including object id maps.  Maybe rename it?
pub(crate) struct WaydaptInputHandler<'a> {
    id_map: IdMap,
    init_info: &'a WaydaptSessionInitInfo,
} // TBD

impl<'a> WaydaptInputHandler<'a> {
    pub(crate) fn new(init_info: &'a WaydaptSessionInitInfo) -> Self {
        let mut s = Self { id_map: IdMap::new(), init_info };
        // the client id map always has the wl_display interface at id 1:
        s.id_map.add(1, init_info.active_interfaces.get_display());
        s
    }
}

struct WaydaptSessionInfo<'a> {
    id_map: &'a mut IdMap,
    init_info: &'a WaydaptSessionInitInfo,
}

impl<'a> SessionInitInfo for WaydaptSessionInfo<'a> {
    fn ucred(&self) -> rustix::net::UCred {
        self.init_info.ucred
    }

    fn get_active_interfaces(&self) -> &'static ActiveInterfaces {
        self.init_info.active_interfaces
    }
}

impl<'a> SessionInfo for WaydaptSessionInfo<'a> {
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

impl<'a> Messenger for WaydaptInputHandler<'a> {
    type FI = VecDeque<OwnedFd>;
    type MS = OutBuffer<IOStream>;

    fn handle(
        &mut self,
        index: usize,
        in_msg: &[u8],
        in_fds: &mut Self::FI,
        outs: &mut [Self::MS; 2], // to: [client,server]
    ) -> IoResult<()> {
        // The demarshalling and remarshalling, along with message handlers:
        let from_server = index > 0;
        let header = MessageHeader::new(in_msg);
        let interface = self.id_map.lookup(header.object_id);
        let msg_decl = interface.get_message(from_server, header.opcode);
        let out = &mut outs[1 - index]; // output opposite side from input
        let active_interfaces = self.init_info.active_interfaces;
        if let Some(handlers) = msg_decl.handlers.get() {
            let mut demarsh = DemarshalledMessage::new(
                header,
                msg_decl,
                in_msg,
                in_fds,
                &mut self.id_map,
                active_interfaces,
            );
            let mut session_info =
                WaydaptSessionInfo { id_map: &mut self.id_map, init_info: self.init_info };
            for h in handlers {
                match h(&mut demarsh, &mut session_info) {
                    MessageHandlerResult::Next => continue,
                    MessageHandlerResult::Send => break,
                    MessageHandlerResult::Drop => return Ok(()),
                }
            }
            demarsh.output(out)?;
        } else if msg_decl.new_id_interface.get().is_some() {
            // Demarshal just to process new_id arg:
            let mut demarsh = DemarshalledMessage::new(
                header,
                msg_decl,
                in_msg,
                in_fds,
                &mut self.id_map,
                active_interfaces,
            );
            demarsh.output_unmodified(out)?;
        } else {
            // No demarshal needed - this path allows transfering data directly from the input
            // buffer to the output buffer with no intermediate copies.
            out.send(in_fds.drain(..), |buf| load_slice(buf, in_msg))?;
        }
        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////

fn wl_registry_global_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // arg0 is the "name" of the global instance, really a uint
    let ArgData::String(interface_name) = msg.get_arg(1) else { panic!() };
    let interface_name = std::str::from_utf8(interface_name).unwrap();
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
    use super::{
        AddHandler, ArgData, Cow, MessageHandlerResult, MessageInfo, OnceLock, SessionInfo,
    };

    static PREFIX: OnceLock<Vec<u8>> = OnceLock::new();

    fn init_handler(args: &[String], adder: &mut dyn AddHandler) {
        // we expect exactly one arg, which is the prefix.  Unlike the C waydapt, the 0th arg is NOT
        // the dll name, it is our first arg.
        assert_eq!(args.len(), 1);
        let prefix = &args[0];
        PREFIX.set(prefix.as_bytes().to_owned()).unwrap();
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
            if let ArgData::String(s) = msg.get_arg_mut(i) {
                let prefix = PREFIX.get().unwrap().as_slice();
                *s = [prefix, s].concat().into();
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
        let prefix = PREFIX.get().unwrap().as_slice();
        let plen = prefix.len();
        for i in 0..msg.get_num_args() {
            match msg.get_arg_mut(i) {
                // Almost all cases will be borrowed, which saves us a copy:
                ArgData::String(Cow::Borrowed(s)) if prefixed(prefix, s) => {
                    *s = &s[plen..]; // O(1) move, no copy
                    return MessageHandlerResult::Next;
                }
                // Not worth optimizing this case - it would only happen if we have a previous handler
                // modifying this same string arg:
                ArgData::String(Cow::Owned(s)) if prefixed(prefix, s) => {
                    s.drain(..plen); // O(N) copy
                    return MessageHandlerResult::Next;
                }
                ArgData::String(_) => return MessageHandlerResult::Drop,
                _ => {}
            }
        }
        panic!("Expected a string arg, didn't find one");
    }
}
