#![warn(clippy::pedantic)]
#![allow(dead_code)]
#![forbid(unsafe_code)]

use crate::input_handler::{IdMap, OutChannel, PopFds};
use crate::map::WL_SERVER_ID_START;
use crate::protocol::{Interface, Message, Type};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

pub(crate) struct MessageHeader {
    pub(crate) object_id: u32,
    pub(crate) opcode: usize,
    pub(crate) size: usize,
}

fn bytes2u32(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[0..4].try_into().unwrap())
}

fn bytes2i32(bytes: &[u8]) -> i32 {
    i32::from_ne_bytes(bytes[0..4].try_into().unwrap())
}

impl MessageHeader {
    pub(crate) fn new(header: &[u8]) -> Self {
        let object_id = bytes2u32(&header[0..4]);
        let word_2 = bytes2u32(&header[4..8]);
        let opcode = (word_2 & 0xffff) as usize;
        let size = (word_2 >> 16) as usize;
        Self { object_id, opcode, size }
    }

    fn output(&self, out: &mut impl OutChannel) -> IoResult<usize> {
        out.push_u32(self.object_id)?;
        #[allow(clippy::cast_possible_truncation)]
        let word_2: u32 = (self.size as u32) << 16 | (self.opcode as u32);
        out.push_u32(word_2)?;
        Ok(8)
    }
}

enum ArgData<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Cow<'a, [u8]>),
    Object(u32),
    NewId(u32, &'static Interface<'static>),
    Array(Cow<'a, [u8]>),
    Fd(usize), // index into fds
}

pub(crate) struct DemarshalledMessage<'a> {
    msg_decl: RMessage,
    header: MessageHeader,
    args: Vec<ArgData<'a>>,
    fds: VecDeque<OwnedFd>,
    source: &'a [u8],
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
    fn add_string(&mut self, data: &'a [u8]) -> usize {
        let len = bytes2u32(&data[0..4]) as usize + 4;
        // should we check that len and the zero termination match? TBD
        self.args.push(ArgData::String(Cow::from(&data[4..len])));
        len
    }
    fn add_array(&mut self, data: &'a [u8]) -> usize {
        let len = bytes2u32(&data[0..4]) as usize + 4;
        self.args.push(ArgData::Array(Cow::from(&data[4..len])));
        len
    }
    fn add_fd(&mut self, fd: OwnedFd) -> usize {
        self.args.push(ArgData::Fd(self.fds.len()));
        self.fds.push_back(fd);
        0
    }
    fn add_new_id(&mut self, data: &'a [u8], id_map: &mut IdMap) -> usize {
        let Some(new_id_interface) = self.msg_decl.get_new_id_interface() else {
            assert!(self.msg_decl.is_wl_registry_bind());
            // The arg is really 3: string, u32, u32
            return self.add_string(data) + self.add_uint(data) + self.add_uint(data);
        };
        let id = bytes2u32(&data[0..4]);
        // check if this is a wayland-idfix delete_id request:
        if id >= WL_SERVER_ID_START && self.msg_decl.is_wl_display_sync() {
            self.args.push(ArgData::Object(id));
        } else {
            id_map.add(id, new_id_interface);
            self.args.push(ArgData::NewId(id, new_id_interface));
        }
        4
    }

    fn init(&mut self, id_map: &mut IdMap, mut data: &'a [u8], fds: &mut impl PopFds) {
        for arg in self.msg_decl.get_args() {
            let arg_len = match arg.typ {
                Type::Int => self.add_int(data),
                Type::Uint => self.add_uint(data),
                Type::Fixed => self.add_fixed(data),
                Type::String => self.add_string(data),
                Type::Object => self.add_object(data),
                Type::NewId => self.add_new_id(data, id_map),
                Type::Array => self.add_array(data),
                Type::Fd => self.add_fd(fds.pop().expect("Not enough FDs")),
            };
            data = &data[arg_len..];
        }
        assert_eq!(self.fds.len(), self.msg_decl.num_fds as usize);
        assert!(data.is_empty());
    }

    pub(crate) fn new(
        header: MessageHeader, from_server: bool, data: &'a [u8], fds: &mut impl PopFds,
        id_map: &mut IdMap,
    ) -> Self {
        assert_eq!(header.size, data.len());
        let interface = id_map.lookup(header.object_id);
        let msg_decl = interface.get_message(from_server, header.opcode);
        let mut s = Self {
            msg_decl,
            header,
            args: Vec::with_capacity(msg_decl.args.len()),
            fds: VecDeque::with_capacity(msg_decl.num_fds as usize),
            source: data,
        };
        s.init(id_map, &data[8..], fds);
        s
    }

    pub(crate) fn output(&mut self, out: &mut impl OutChannel) -> IoResult<usize> {
        #![allow(clippy::cast_sign_loss)]
        self.output_all_fds(out)?;
        let mut size: usize = self.header.output(out)?;
        for arg in &mut self.args {
            size += match arg {
                ArgData::Int(i) => out.push_u32(*i as u32),
                ArgData::Uint(u) => out.push_u32(*u),
                ArgData::Fixed(f) => out.push_u32(*f as u32),
                ArgData::String(s) => out.push_array(s, true),
                ArgData::Object(id) | ArgData::NewId(id, _) => out.push_u32(*id),
                ArgData::Array(a) => out.push_array(a, false),
                ArgData::Fd(_) => Ok(0),
            }?;
        }
        assert_eq!(size, self.header.size);
        Ok(size)
    }

    fn output_all_fds(&mut self, out: &mut impl OutChannel) -> IoResult<()> {
        loop {
            let Some(fd) = self.fds.pop_front() else { break };
            out.push_fd(fd)?;
        }
        Ok(())
    }

    pub(crate) fn output_all_unmodified(&mut self, out: &mut impl OutChannel) -> IoResult<usize> {
        self.output_all_fds(out)?;
        out.push(self.source)
    }
}

// How do lifetimes figure into message handling?  Suppose we want to handle a message and alter a
// string arg.  If we can send the message during the lifetime of the string, then we don't need to
// copy it.  Also, how would we copy it - the DemarshalledMessage<'a> lifetime will be longer than
// anything created during the handler.  Maybe the string and array args should contain Cows?

// For the string arg, there's really 3 states.  One is raw str pointing into the buffer.  The
// second and third is a Cow<'_, str> which is formed using from_utf8_lossy from the raw str.  But
// what if we don't want any utf8 conversion?  The Wayland protocol uses utf8 strings, though.  But
// the handler might want to do something that doesn't lose any content in the string even if it
// isn't legal utf8, like add a prefix.  So maybe a Cow<'a, [u8]>, with the responsibility on the
// handler if it wants to deal with utf8, it has to convert back to u8.

// The format of the string is 0-terminated, with the len not including the 0, but the size padded
// to 4 bytes.  We will assume that if we demarshal the message for handlers, that the handlers can
// modify the demarshalled message - so no output_all_unmodified optimization in this code path.

// For fds, we can also use a Cow in the Fd arg, keeping a BorrowedFd there initially with the
// matching OwnedFd in the vec.  Does that work?  No, we cannot have both an OwnedFd that is free to
// be moved and a BorrowedFd on it.  Instead, keep the vec of Owned and the Fd arg indexing into
// that, and use std::mem::swap from handlers to set it.  What if we ever want the handler to
// examine the fd by borrowing it?  It can call try_clone on it without swapping it - and maybe swap
// it later.

// Another possibility is that we store the Args in a VecDeque and pop them on output.  Then the Fds
// can contain OwnedFds, and handlers can swap them, and we don't need a separate fd VecDeque.
