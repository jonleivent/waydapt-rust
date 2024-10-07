// everything in here is under #[cfg(test)]
#![allow(unused)]
#![cfg_attr(coverage_nightly, coverage(off))]

use crate::buffers::privates::{mk_extend_chunk, Chunk};
use crate::buffers::ExtendChunk;
use crate::crate_traits::Messenger;
use crate::for_handlers::{RInterface, SessionInfo, SessionInitInfo};
use crate::header::MessageHeader;
use crate::postparse::ActiveInterfaces;
pub(crate) use crate::protocol::{Arg, Type};
use crate::protocol::{Interface, Message};

use std::io::Result as IoResult;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::{collections::VecDeque, fs::File};

type RMessage = &'static Message<'static>;

// create a unique fd by creating a temp file and writing uid to it
pub(crate) fn fd_for_test(uid: u32) -> OwnedFd {
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&uid.to_ne_bytes()).unwrap();
    f.into()
}

// check fd from fd_for_test
pub(crate) fn check_test_fd(fd: impl AsFd, uid: u32) -> bool {
    let ofd = fd.as_fd().try_clone_to_owned().unwrap();
    let mut f = File::from(ofd);
    let mut v = [0u8; 4];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut v[..]).unwrap();
    uid == u32::from_ne_bytes(v)
}

#[derive(Debug, Clone)]
pub(crate) enum FakeMsgArgData<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(&'a [u8]),
    Object(u32),
    NewId(u32, Option<RInterface>),
    Array(&'a [u8]),
    Fd(u32),
}

pub(crate) struct FakeMsgMaker<'a> {
    object_id: u32,
    opcode: u16,
    args: Vec<(String, FakeMsgArgData<'a>)>,
}

fn mkarg(name: String, typ: Type) -> Arg { Arg { name, typ, interface_name: None } }

impl<'a> FakeMsgMaker<'a> {
    pub(crate) fn new(object_id: u32, opcode: u16) -> Self {
        Self { object_id, opcode, args: Vec::new() }
    }

    pub(crate) fn produce(&self, chunk: &mut Chunk) -> (RMessage, VecDeque<OwnedFd>) {
        chunk.clear();
        let mut ec = mk_extend_chunk(chunk);
        ec.add_u32(0);
        ec.add_u32(0); // room for header
        let msg = Box::leak(Box::new(Message::new(u32::from(self.opcode))));
        let mut fds = VecDeque::new();
        for (name, a) in self.args.clone() {
            match a {
                FakeMsgArgData::Int(i) => {
                    ec.add_i32(i);
                    msg.args.push(mkarg(name, Type::Int));
                }
                FakeMsgArgData::Uint(u) => {
                    ec.add_u32(u);
                    msg.args.push(mkarg(name, Type::Uint));
                }
                FakeMsgArgData::Fixed(f) => {
                    ec.add_i32(f);
                    msg.args.push(mkarg(name, Type::Fixed));
                }
                FakeMsgArgData::String(s) => {
                    ec.add_array(s);
                    msg.args.push(mkarg(name, Type::String));
                }
                FakeMsgArgData::Object(u) => {
                    ec.add_u32(u);
                    msg.args.push(mkarg(name, Type::Object));
                }
                FakeMsgArgData::NewId(u, interface) => {
                    ec.add_u32(u);
                    let interface_name = interface.map(|i| i.name.clone());
                    msg.args.push(Arg { name, typ: Type::NewId, interface_name });
                    if let Some(i) = interface {
                        msg.new_id_interface.set(i).unwrap();
                    }
                }
                FakeMsgArgData::Array(a) => {
                    ec.add_array(a);
                    msg.args.push(mkarg(name, Type::Array));
                }
                FakeMsgArgData::Fd(u) => {
                    msg.args.push(mkarg(name, Type::Fd));
                    fds.push_back(fd_for_test(u));
                }
            }
        }
        let object_id = self.object_id;
        let opcode = self.opcode;
        #[allow(clippy::cast_possible_truncation)]
        let words = chunk.len() as u16;
        let hdr = MessageHeader { object_id, opcode, size: words * 4 }.as_words();
        chunk[0] = hdr[0];
        chunk[1] = hdr[1];
        (msg, fds)
    }

    pub(crate) fn add_int(&mut self, name: impl Into<String>, data: i32) {
        self.args.push((name.into(), FakeMsgArgData::Int(data)));
    }

    pub(crate) fn add_uint(&mut self, name: impl Into<String>, data: u32) {
        self.args.push((name.into(), FakeMsgArgData::Uint(data)));
    }

    pub(crate) fn add_fixed(&mut self, name: impl Into<String>, data: i32) {
        self.args.push((name.into(), FakeMsgArgData::Fixed(data)));
    }

    pub(crate) fn add_string(&mut self, name: impl Into<String>, data: &'a [u8]) {
        self.args.push((name.into(), FakeMsgArgData::String(data)));
    }

    pub(crate) fn add_array(&mut self, name: impl Into<String>, data: &'a [u8]) {
        self.args.push((name.into(), FakeMsgArgData::Array(data)));
    }

    pub(crate) fn add_fd(&mut self, name: impl Into<String>, data: u32) {
        self.args.push((name.into(), FakeMsgArgData::Fd(data)));
    }

    pub(crate) fn add_object(&mut self, name: impl Into<String>, data: u32) {
        self.args.push((name.into(), FakeMsgArgData::Object(data)));
    }

    pub(crate) fn add_new_id(
        &mut self, name: impl Into<String>, data: u32, interface: Option<RInterface>,
    ) {
        self.args.push((name.into(), FakeMsgArgData::NewId(data, interface)));
    }

    pub(crate) fn mod_scalar_arg(&mut self, i: usize, new_data: u32) {
        #![allow(clippy::cast_possible_wrap)]
        let (_, a) = &mut self.args[i];
        match a {
            FakeMsgArgData::Int(i) | FakeMsgArgData::Fixed(i) => *i = new_data as i32,
            FakeMsgArgData::Uint(u) | FakeMsgArgData::Object(u) | FakeMsgArgData::NewId(u, _) => {
                *u = new_data;
            }
            _ => panic!(),
        }
    }

    pub(crate) fn mod_vector_arg(&mut self, i: usize, new_data: &'a [u8]) {
        let (_, a) = &mut self.args[i];
        match a {
            FakeMsgArgData::String(a) | FakeMsgArgData::Array(a) => *a = new_data,
            _ => panic!(),
        }
    }
}

pub(crate) struct FakeSessionInfo(pub(crate) Vec<(u32, RInterface)>);

impl SessionInitInfo for FakeSessionInfo {
    fn ucred(&self) -> Option<rustix::net::UCred> { None }
    fn get_active_interfaces(&self) -> &'static ActiveInterfaces { todo!() }
    fn get_display(&self) -> RInterface { todo!() } // wl_display
    fn get_debug_level(&self) -> u32 { 0 }
}

impl SessionInfo for FakeSessionInfo {
    fn try_lookup(&self, _id: u32) -> Option<RInterface> { None }
    fn lookup(&self, _id: u32) -> RInterface { todo!() }
    fn add(&mut self, id: u32, interface: RInterface) { self.0.push((id, interface)); }
    fn delete(&mut self, _id: u32) {}
}

pub(crate) struct FakeMessenger(pub(crate) Chunk, pub(crate) Vec<OwnedFd>);

impl Messenger for FakeMessenger {
    fn send(
        &mut self, fds: impl IntoIterator<Item = OwnedFd>,
        msgfun: impl FnOnce(ExtendChunk) -> MessageHeader,
    ) -> IoResult<usize> {
        #![allow(clippy::cast_possible_truncation)]
        self.0.push(0);
        self.0.push(0); // room for header
        let mut hdr = msgfun(mk_extend_chunk(&mut self.0));
        hdr.size = self.0.len() as u16 * 4;
        let w = hdr.as_words();
        self.0[0] = w[0];
        self.0[1] = w[1];
        self.1.extend(fds);
        Ok(self.0.len())
    }

    fn send_raw(
        &mut self, _fds: impl IntoIterator<Item = OwnedFd>, _raw_msg: &[u32],
    ) -> IoResult<usize> {
        todo!()
    }
}
