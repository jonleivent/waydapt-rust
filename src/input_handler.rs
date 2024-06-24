#![warn(clippy::pedantic)]
#![allow(dead_code)]
#![forbid(unsafe_code)]

use crate::map::{WaylandObjectMap, WL_SERVER_ID_START};
use crate::message::DemarshalledMessage;
use crate::protocol::Interface;
use crate::session::WaydaptSessionInitInfo;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

pub(crate) trait OutChannel {
    fn push_fd(&mut self, fd: OwnedFd) -> IoResult<usize>;

    fn push_u32(&mut self, data: u32) -> IoResult<usize>;

    fn push_array(&mut self, data: &[u8]) -> IoResult<usize>;

    fn end_msg(&mut self) -> IoResult<usize>;

    fn push(&mut self, msg: &[u8]) -> IoResult<usize>;
}

pub(crate) trait PopFds {
    fn pop(&mut self) -> Option<OwnedFd>;
}

pub(crate) trait InputHandler {
    fn handle<P, C>(
        &mut self, index: usize, in_msg: &[u8], in_fds: &mut P, outs: &mut [C; 2],
    ) -> IoResult<()>
    where
        P: PopFds,
        C: OutChannel;
}

////////////////////////////////////////////////////////////////////////////////

impl PopFds for VecDeque<OwnedFd> {
    fn pop(&mut self) -> Option<OwnedFd> {
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

    pub(crate) fn lookup(&self, id: u32) -> &'static Interface<'static> {
        let i = if id >= WL_SERVER_ID_START {
            self.server_id_map.lookup(id)
        } else {
            self.client_id_map.lookup(id)
        };
        i.unwrap_or_else(|| panic!("No interface for object id {id}"))
    }

    pub(crate) fn add(&mut self, id: u32, interface: &'static Interface<'static>) {
        if id >= WL_SERVER_ID_START {
            self.server_id_map.add(id, interface);
        } else {
            self.client_id_map.add(id, interface);
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

impl<'a> InputHandler for WaydaptInputHandler<'a> {
    fn handle<P, C>(
        &mut self,
        index: usize,
        in_msg: &[u8],
        in_fds: &mut P,
        outs: &mut [C; 2], // to: [client,server]
    ) -> IoResult<()>
    where
        P: PopFds,
        C: OutChannel,
    {
        // The demarshalling and remarshalling, along with message handlers:
        let from_server = index > 0;
        let header = crate::message::MessageHeader::new(in_msg);
        let interface = self.id_map.lookup(header.object_id);
        let msg_decl = if from_server {
            interface.events[header.opcode]
        } else {
            interface.requests[header.opcode]
        };
        if let Some(handlers) = msg_decl.handlers.get() {
            todo!()
        } else if msg_decl.new_id_interface.get().is_some() {
            // Demarshal just to process new_id_interface:
            let out = &mut outs[1 - index];
            let mut demarsh =
                DemarshalledMessage::new(header, from_server, in_msg, in_fds, &mut self.id_map);
            demarsh.output_all_unmodified(out)?;
        } else {
            // No demarshal needed
            let out = &mut outs[1 - index];
            for _ in 0..msg_decl.num_fds {
                out.push_fd(in_fds.pop().unwrap())?;
            }
            out.push(in_msg)?;
            out.end_msg()?;
        }
        todo!()
    }
}
