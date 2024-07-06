#![warn(clippy::pedantic)]
#![allow(dead_code)]
//#![forbid(unsafe_code)]

use arrayvec::ArrayVec;

use crate::basics::{bytes2i32, bytes2u32, round4, MAX_ARGS};
use crate::crate_traits::{FdInput, MessageSender};
use crate::for_handlers::MessageInfo;
use crate::header::MessageHeader;
use crate::input_handler::IdMap;
use crate::map::WL_SERVER_ID_START;
use crate::postparse::ActiveInterfaces;
use crate::protocol::{Interface, Message, Type};
use std::borrow::Cow;
use std::cell::Cell;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

#[derive(Debug)]
pub enum ArgData<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Cow<'a, [u8]>),
    Object(u32),
    NewId { id: u32, interface: &'static Interface<'static> },
    Array(Cow<'a, [u8]>),
    Fd { index: usize },
}

pub(crate) struct DemarshalledMessage<'a> {
    msg_decl: RMessage,
    header: MessageHeader,
    args: ArrayVec<ArgData<'a>, MAX_ARGS>,
    source: &'a [u8],
    maybe_modified: bool,
    maybe_arg_type_changed: bool,
    fds: ArrayVec<OwnedFd, MAX_ARGS>,
}

type RMessage = &'static Message<'static>;

impl<'a> DemarshalledMessage<'a> {
    fn add_int(&mut self, data: &[u8]) -> usize {
        self.args.push(ArgData::Int(bytes2i32(data)));
        4
    }
    fn add_uint(&mut self, data: &[u8]) -> usize {
        self.args.push(ArgData::Uint(bytes2u32(data)));
        4
    }
    fn add_fixed(&mut self, data: &[u8]) -> usize {
        self.args.push(ArgData::Fixed(bytes2i32(data)));
        4
    }
    fn add_object(&mut self, data: &[u8]) -> usize {
        self.args.push(ArgData::Object(bytes2u32(data)));
        4
    }
    fn add_string_internal(&mut self, data: &'a [u8]) -> (usize, &'a [u8]) {
        let strlen = bytes2u32(&data[0..4]) as usize;
        let len = round4(strlen + 1) + 4; // +1 for 0-term
        let s = &data[4..len];
        // Check that the first 0 in s is at strlen:
        let zero_term =
            s.iter().enumerate().find_map(|(c, i)| if c == 0 { Some(*i as usize) } else { None });
        assert_eq!(zero_term, Some(strlen));
        self.args.push(ArgData::String(Cow::from(s)));
        (len, s)
    }
    fn add_string(&mut self, data: &'a [u8]) -> usize {
        self.add_string_internal(data).0
    }

    fn add_array(&mut self, data: &'a [u8]) -> usize {
        let len = round4(bytes2u32(&data[0..4]) as usize) + 4;
        self.args.push(ArgData::Array(Cow::from(&data[4..len])));
        len
    }
    fn add_fd(&mut self, fd: OwnedFd) -> usize {
        self.args.push(ArgData::Fd { index: self.fds.len() });
        self.fds.push(fd);
        0
    }
    fn add_new_id(
        &mut self, data: &'a [u8], id_map: &'_ mut IdMap,
        active_interfaces: &'static ActiveInterfaces,
    ) -> usize {
        let Some(interface) = self.msg_decl.get_new_id_interface() else {
            return self.add_wl_registry_bind_new_id(data, id_map, active_interfaces);
        };
        let id = bytes2u32(&data[0..4]);
        // check if this is a wayland-idfix delete_id request:
        if id >= WL_SERVER_ID_START && self.msg_decl.is_wl_display_sync() {
            // TBD should we do the delete here?: Note that the C waydapt doesn't.  So we won't either.

            //id_map.delete(id);

            //There's also the return handshake request, which is a sync with a high id that doesn't
            // exist.  But the delete method on id_maps does nothing if the id doesn't exist.

            // Note that, like the add_wl_registry_bind_new_id case, since we are pushing an arg
            // type that is different from the expected one, if a handler modifies this message, it
            // will fail checks in output_modified.  It can only be sent via output_unmodified.  By
            // pushing an Object arg, it makes unprepared user handlers more likely to fail in a
            // predictable way.
            self.args.push(ArgData::Object(id));
        } else {
            id_map.add(id, interface);
            self.args.push(ArgData::NewId { id, interface });
        }
        4
    }

    fn add_wl_registry_bind_new_id(
        &mut self, data: &'a [u8], id_map: &'_ mut IdMap,
        active_interfaces: &'static ActiveInterfaces,
    ) -> usize {
        // Right now, only wl_registry::bind can have such an arg, but we could potentially handle
        // others.
        assert!(self.msg_decl.is_wl_registry_bind());
        // Marshalling through output_modified it will fail because there will be a mismatch between
        // the msg_decl args and the args vector for this argument.  So it has to go through the
        // output_unmodified method.  Maybe fix this if needed?  TBD

        // The arg is really 3: string, u32, u32, which are interface-name, version, new_id.
        let (len, s) = self.add_string_internal(data);
        let data = &data[len..];
        let name = std::str::from_utf8(s).unwrap();
        let interface = active_interfaces.get_global(name);
        let version = bytes2u32(data);
        let len = len + 4;
        self.args.push(ArgData::Uint(version));
        let data = &data[4..];
        let id = bytes2u32(data);
        let len = len + 4;
        let limited_version = *interface.version().unwrap(); // if none, then not active
        assert!(
            version <= limited_version,
            "Client attempted to bind version {version} > {limited_version} on {interface}"
        );
        id_map.add(id, interface);
        self.args.push(ArgData::NewId { id, interface });
        len
    }

    fn init(
        &mut self, id_map: &'_ mut IdMap, mut data: &'a [u8], fds: &'_ mut impl FdInput,
        active_interfaces: &'static ActiveInterfaces,
    ) {
        for arg in self.msg_decl.get_args() {
            let arg_len = match arg.typ {
                Type::Int => self.add_int(data),
                Type::Uint => self.add_uint(data),
                Type::Fixed => self.add_fixed(data),
                Type::String => self.add_string(data),
                Type::Object => self.add_object(data),
                Type::NewId => self.add_new_id(data, id_map, active_interfaces),
                Type::Array => self.add_array(data),
                Type::Fd => self.add_fd(fds.try_take_fd().expect("Not enough FDs")),
            };
            data = &data[arg_len..];
        }
        assert!(data.is_empty());
    }

    #[cold]
    pub(crate) fn new(
        header: MessageHeader, msg_decl: RMessage, data: &'a [u8], fds: &'_ mut impl FdInput,
        id_map: &'_ mut IdMap, active_interfaces: &'static ActiveInterfaces,
    ) -> Self {
        assert_eq!(header.size as usize, data.len());
        let mut s = Self {
            msg_decl,
            header,
            args: ArrayVec::new(),
            source: data,
            maybe_modified: false,
            maybe_arg_type_changed: false,
            fds: ArrayVec::new(),
        };
        s.init(id_map, &data[8..], fds, active_interfaces);
        s
    }

    fn test_arg_invariant(&self) {
        assert!(
            self.msg_decl.args.len() == self.args.len()
                || (self.msg_decl.is_wl_registry_bind()
                    && self.msg_decl.args.len() + 2 == self.args.len())
        );
    }

    fn output_modified_check_types(self, sender: &mut impl MessageSender) -> IoResult<usize> {
        self.test_arg_invariant();
        sender.send(self.fds, |mut ext| {
            for arg_typ in self.args.into_iter().zip(self.msg_decl.args.iter().map(|a| a.typ)) {
                match arg_typ {
                    (ArgData::Int(i), Type::Int) => ext.add_i32(i),
                    (ArgData::Uint(u), Type::Uint) => ext.add_u32(u),
                    (ArgData::Fixed(f), Type::Fixed) => ext.add_i32(f),
                    (ArgData::String(s), Type::String) => ext.add_array(&s, true),
                    (ArgData::Object(i), Type::Object) => ext.add_u32(i),
                    (ArgData::NewId { id, .. }, Type::NewId) => ext.add_u32(id),
                    (ArgData::Array(a), Type::Array) => ext.add_array(&a, false),
                    (ArgData::Fd { .. }, Type::Fd) => {} // TBD: check index?
                    (arg, typ) => panic!("{arg:?} vs. {typ:?} mismatch"),
                };
            }
            self.header
        })
    }

    fn output_modified(self, sender: &mut impl MessageSender) -> IoResult<usize> {
        sender.send(self.fds, |mut ext| {
            for arg in self.args {
                match arg {
                    ArgData::Int(i) => ext.add_i32(i),
                    ArgData::Uint(u) => ext.add_u32(u),
                    ArgData::Fixed(f) => ext.add_i32(f),
                    ArgData::String(s) => ext.add_array(&s, true),
                    ArgData::Object(i) => ext.add_u32(i),
                    ArgData::NewId { id, .. } => ext.add_u32(id),
                    ArgData::Array(a) => ext.add_array(&a, false),
                    ArgData::Fd { .. } => {} // TBD: check index?
                };
            }
            self.header
        })
    }

    pub(crate) fn output(self, sender: &mut impl MessageSender) -> IoResult<usize> {
        if self.maybe_arg_type_changed {
            self.output_modified_check_types(sender)
        } else if self.maybe_modified {
            self.output_modified(sender)
        } else {
            self.output_unmodified(sender)
        }
    }

    pub(crate) fn output_unmodified(self, sender: &mut impl MessageSender) -> IoResult<usize> {
        debug_assert!(!self.maybe_modified);
        sender.send_raw(self.fds, self.source)
    }
}

impl<'a> MessageInfo<'a> for DemarshalledMessage<'a> {
    fn get_num_args(&self) -> usize {
        self.args.len()
    }

    fn get_arg(&self, index: usize) -> &ArgData<'a> {
        &self.args[index]
    }

    fn get_arg_mut(&mut self, index: usize) -> &mut ArgData<'a> {
        self.maybe_modified = true;
        self.maybe_arg_type_changed = true;
        &mut self.args[index]
    }

    fn get_cell_args(&mut self) -> &[Cell<ArgData<'a>>] {
        self.maybe_modified = true;
        self.maybe_arg_type_changed = true;
        Cell::from_mut(self.args.as_mut_slice()).as_slice_of_cells()
    }

    fn get_decl(&self) -> &'static Message<'static> {
        self.msg_decl
    }

    fn get_object_id(&self) -> u32 {
        self.header.object_id
    }

    fn set_arg(&mut self, index: usize, a: ArgData<'a>) {
        use std::mem::discriminant as discr;
        let sa = &mut self.args[index];
        assert_eq!(discr(sa), discr(&a), "{sa:?} vs. {a:?} mismatch");
        self.maybe_modified = true;
        *sa = a;
    }
}
