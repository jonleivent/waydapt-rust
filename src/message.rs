#![warn(clippy::pedantic)]
#![allow(dead_code)]
//#![forbid(unsafe_code)]

use arrayvec::ArrayVec;

use crate::basics::{bytes2i32, bytes2u32, load_slice, round4, MAX_ARGS, MAX_BYTES_OUT};
use crate::crate_traits::{FdInput, MessageSender};
use crate::for_handlers::MessageInfo;
use crate::input_handler::IdMap;
use crate::map::WL_SERVER_ID_START;
use crate::postparse::ActiveInterfaces;
use crate::protocol::{Interface, Message, Type};
use std::borrow::Cow;
use std::cell::Cell;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

pub(crate) struct MessageHeader {
    pub(crate) object_id: u32,
    pub(crate) opcode: usize,
    pub(crate) size: usize,
}

impl MessageHeader {
    pub(crate) fn new(header: &[u8]) -> Self {
        let object_id = bytes2u32(&header[0..4]);
        let word_2 = bytes2u32(&header[4..8]);
        let opcode = (word_2 & 0xffff) as usize;
        let size = (word_2 >> 16) as usize;
        assert!(size <= MAX_BYTES_OUT);
        Self { object_id, opcode, size }
    }

    fn as_bytes(&self) -> [u8; 8] {
        let word1 = self.object_id.to_ne_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let word2 = (((self.size << 16) | self.opcode) as u32).to_ne_bytes();
        [word1, word2].concat().try_into().unwrap()
    }
}

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
    fds: ArrayVec<OwnedFd, MAX_ARGS>,
}

// Maybe, instead of storing the OwnedFds in the Fd args, we have a separate vector for the fds as a
// field in DemarshalledMessage and store an index into that in the Fd arg.  That way we can push
// the fds out before any part of the message, assuming we push message parts directly to the out
// buffer.  Or we can store BorrowedFds in the Fd Args.

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

    pub(crate) fn new(
        header: MessageHeader, msg_decl: RMessage, data: &'a [u8], fds: &'_ mut impl FdInput,
        id_map: &'_ mut IdMap, active_interfaces: &'static ActiveInterfaces,
    ) -> Self {
        assert_eq!(header.size, data.len());
        let mut s = Self {
            msg_decl,
            header,
            args: ArrayVec::new(),
            source: data,
            maybe_modified: false,
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

    fn recalculate_size(&mut self) {
        let mut size = 8; // for the header
        for arg in &self.args {
            size += match arg {
                ArgData::Fd { .. } => 0,
                ArgData::Array(a) => 4 + round4(a.len()),
                ArgData::String(s) => 4 + round4(s.len() + 1), // +1 for 0 term
                _ => 4,
            }
        }
        assert!(size <= MAX_BYTES_OUT);
        self.header.size = size;
    }

    fn push_u32(x: u32, buf: &mut [u8]) -> usize {
        buf[..4].copy_from_slice(&x.to_ne_bytes());
        4
    }

    fn push_i32(x: i32, buf: &mut [u8]) -> usize {
        buf[..4].copy_from_slice(&x.to_ne_bytes());
        4
    }

    fn push_array(data: &[u8], zero_term: bool, buf: &mut [u8]) -> usize {
        let len = data.len();
        let lenz = len + usize::from(zero_term);
        let lenz4 = round4(lenz);
        let (len_field, out) = buf.split_at_mut(4);
        #[allow(clippy::cast_possible_truncation)]
        // does the length pushed for strings include the 0-term?  Yes:
        len_field.copy_from_slice(&(lenz as u32).to_ne_bytes());
        let (out, pad) = out[..lenz4].split_at_mut(len);
        out.copy_from_slice(data);
        pad.fill(0);
        lenz4 + 4
    }

    fn output_modified(&mut self, sender: &mut impl MessageSender) -> IoResult<usize> {
        self.test_arg_invariant();
        sender.send(self.fds.drain(..), |out| {
            let (hbuf, mut buf) = out.split_at_mut(8);
            let mut message_size = 8; // for the header
            for arg_typ in self.args.drain(..).zip(self.msg_decl.args.iter().map(|a| a.typ)) {
                let arg_size = match arg_typ {
                    (ArgData::Int(i), Type::Int) => Self::push_i32(i, buf),
                    (ArgData::Uint(u), Type::Uint) => Self::push_u32(u, buf),
                    (ArgData::Fixed(f), Type::Fixed) => Self::push_i32(f, buf),
                    (ArgData::String(s), Type::String) => Self::push_array(&s, true, buf),
                    (ArgData::Object(i), Type::Object) => Self::push_u32(i, buf),
                    (ArgData::NewId { id, .. }, Type::NewId) => Self::push_u32(id, buf),
                    (ArgData::Array(a), Type::Array) => Self::push_array(&a, false, buf),
                    (ArgData::Fd { .. }, Type::Fd) => 0, // TBD: check index?
                    (arg, typ) => panic!("{arg:?} vs. {typ:?} mismatch"),
                };
                message_size += arg_size;
                buf = &mut buf[arg_size..];
            }
            self.header.size = message_size;
            hbuf.copy_from_slice(&self.header.as_bytes());
            message_size
        })
    }

    pub(crate) fn output(&mut self, sender: &mut impl MessageSender) -> IoResult<usize> {
        if self.maybe_modified {
            self.output_modified(sender)
        } else {
            self.output_unmodified(sender)
        }
    }

    pub(crate) fn output_unmodified(&mut self, sender: &mut impl MessageSender) -> IoResult<usize> {
        debug_assert!(!self.maybe_modified);
        sender.send(self.fds.drain(..), |out| load_slice(out, self.source))
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
        &mut self.args[index]
    }

    fn get_cell_args(&mut self) -> &[Cell<ArgData<'a>>] {
        self.maybe_modified = true;
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
