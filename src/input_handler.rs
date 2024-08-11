#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use crate::basics::round4;
use crate::buffers::OutBuffer;
use crate::crate_traits::{ClientPeer, FdInput, Messenger, ServerPeer};
#[allow(clippy::enum_glob_use)]
use crate::for_handlers::{MessageHandlerResult::*, RInterface, SessionInfo, SessionInitInfo};
use crate::header::MessageHeader;
use crate::map::{WaylandObjectMap, WL_SERVER_ID_START};
use crate::message::DemarshalledMessage;
use crate::postparse::ActiveInterfaces;
use crate::protocol::{Message, Type};
use crate::session::WaydaptSessionInitInfo;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

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
        self.init_info.ucred()
    }

    fn get_active_interfaces(&self) -> &'static ActiveInterfaces {
        self.init_info.get_active_interfaces()
    }

    fn get_display(&self) -> RInterface {
        self.init_info.get_display()
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
        s.id_map.add(1, init_info.get_display());
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
        } else if self.init_info.options.debug_level == 0 {
            // Fast track, no demarshalling, although maybe just a little parsing to find the new_id
            // value if it is present:
            if let Some(interface) = msg_decl.new_id_interface.get() {
                let id = Self::find_new_id(msg_decl, in_msg);
                self.add(id, interface);
            } else {
                // We can't use new_id_interface or find_new_id on wl_registry::bind, but we
                // shouldn't need to, because it always has a builtin handler, hence will be handled
                // in the top part of this if.
                debug_assert!(!msg_decl.is_wl_registry_bind());
            }
            out.send_raw(in_fds.drain(msg_decl.num_fds as usize), in_msg)?;
        } else {
            // Demarshal just for debug output (and new_id, if present):
            let mut dmsg = DemarshalledMessage::new(header, msg_decl, in_msg);
            dmsg.demarshal(in_fds, self);
            self.debug_unified(header, msg_decl, &dmsg, from_server);
            dmsg.relay_unmodified(out)?;
        }

        Ok(())
    }

    fn find_new_id(msg_decl: &Message, data: &[u32]) -> u32 {
        // The new_id arg is most often the first arg, so this is usually fast
        let mut i = 2; // bypass 2 header words
        for arg in msg_decl.get_args() {
            i += match arg.typ {
                Type::NewId => return data[i],
                Type::Int | Type::Uint | Type::Fixed | Type::Object => 1,
                Type::String | Type::Array => round4(data[i] as usize) / 4 + 1,
                Type::Fd => 0,
            };
        }
        panic!("No new_id arg found when one was expected for {msg_decl:?}")
    }
}

mod debug {
    use super::{DemarshalledMessage, Mediator, Message, MessageHeader};

    impl<'a> Mediator<'a> {
        fn eprint_flow_unified(&self, from_server: bool) {
            if from_server {
                eprint!("server->waydapt->client[{}] ", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("client[{}]->waydapt->server ", self.init_info.ucred.pid.as_raw_nonzero());
            }
        }

        fn eprint_flow_in(&self, from_server: bool) {
            if from_server {
                eprint!("server->waydapt[{}] ", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("client[{}]->waydapt ", self.init_info.ucred.pid.as_raw_nonzero());
            }
        }

        fn eprint_flow_out(&self, from_server: bool) {
            if from_server {
                eprint!("waydapt->client[{}] ", self.init_info.ucred.pid.as_raw_nonzero());
            } else {
                eprint!("waydapt[{}]->server ", self.init_info.ucred.pid.as_raw_nonzero());
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
