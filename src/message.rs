#![warn(clippy::pedantic)]
#![allow(dead_code)]
#![forbid(unsafe_code)]

use crate::basics::{round4, to_u8_slice};
use crate::crate_traits::{FdInput as FI, Messenger as MO};
use crate::for_handlers::{MessageInfo, SessionInfo as Info};
use crate::header::MessageHeader;
use crate::map::WL_SERVER_ID_START;
use crate::protocol::{Interface, Message, Type};
use rustix::fd::AsRawFd;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::ffi::CStr;
use std::io::Result as IoResult;
use std::os::unix::io::OwnedFd;

type RMessage = &'static Message<'static>;
type RInterface = &'static Interface<'static>;

#[derive(Debug)]
pub enum ArgData<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Cow<'a, CStr>),
    Object(u32),
    NewId { id: u32, interface: RInterface },
    Array(Cow<'a, [u8]>),
    Fd { index: usize },
}

type InlineVec<T> = SmallVec<[T; 2]>;

pub(crate) struct DemarshalledMessage<'a> {
    msg_decl: RMessage,
    header: MessageHeader,
    args: InlineVec<ArgData<'a>>,
    source: &'a [u32],
    maybe_modified: bool,
    maybe_arg_type_changed: bool,
    fds: InlineVec<OwnedFd>,
}

impl<'a> DemarshalledMessage<'a> {
    fn add_int(&mut self, data: &[u32]) -> usize {
        #[allow(clippy::cast_possible_wrap)]
        self.args.push(ArgData::Int(data[0] as i32));
        1
    }

    fn add_uint(&mut self, data: &[u32]) -> usize {
        self.args.push(ArgData::Uint(data[0]));
        1
    }

    fn add_fixed(&mut self, data: &[u32]) -> usize {
        #[allow(clippy::cast_possible_wrap)]
        self.args.push(ArgData::Fixed(data[0] as i32));
        1
    }

    fn add_object(&mut self, data: &[u32]) -> usize {
        self.args.push(ArgData::Object(data[0]));
        1
    }

    fn add_string_internal(&mut self, data: &'a [u32]) -> (usize, &'a CStr) {
        // in Wayland wire protocol, the strlen includes the 0-term, unlike in C
        let strlen = data[0] as usize;
        let end = round4(strlen) + 4;
        let nwords = end / 4;
        let s = to_u8_slice(&data[1..nwords]);
        // Check that the first 0 in s is at strlen:
        let c = CStr::from_bytes_with_nul(&s[..strlen]).unwrap();
        self.args.push(ArgData::String(Cow::from(c)));
        (nwords, c)
    }

    fn add_string(&mut self, data: &'a [u32]) -> usize { self.add_string_internal(data).0 }

    fn add_array(&mut self, data: &'a [u32]) -> usize {
        let len = data[0] as usize;
        let end = round4(len) + 4;
        let nwords = end / 4;
        let s = to_u8_slice(&data[1..nwords]);
        self.args.push(ArgData::Array(Cow::from(&s[..len])));
        nwords
    }

    fn add_fd(&mut self, fd: OwnedFd) -> usize {
        self.args.push(ArgData::Fd { index: self.fds.len() });
        self.fds.push(fd);
        0
    }

    fn add_new_id(&mut self, data: &'a [u32], info: &mut impl Info) -> usize {
        let Some(interface) = self.msg_decl.get_new_id_interface() else {
            return self.add_wl_registry_bind_new_id(data, info);
        };
        let id = data[0];
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
            info.add(id, interface);
            self.args.push(ArgData::NewId { id, interface });
        }
        1
    }

    fn add_wl_registry_bind_new_id(&mut self, data: &'a [u32], info: &mut impl Info) -> usize {
        // Right now, only wl_registry::bind can have such an arg, but we could potentially handle
        // others.
        assert!(self.msg_decl.is_wl_registry_bind());
        // Marshalling through output_modified will fail because there will be a mismatch between
        // the msg_decl args and the args vector for this argument.  So it has to go through the
        // output_unmodified method.  Maybe fix this if needed?  TBD

        // The arg is really 3: string, u32, u32, which are interface-name, version, new_id.
        let (len, s) = self.add_string_internal(data);
        let rest = &data[len..];
        let name = s.to_str().unwrap();
        let interface = info.get_active_interfaces().get_global(name);
        let version = rest[0];
        let id = rest[1];
        let limited_version = *interface.version().unwrap(); // if none, then not active
        assert!(
            version <= limited_version,
            "Client attempted to bind version {version} > {limited_version} on {interface}"
        );
        info.add(id, interface);
        self.args.push(ArgData::Uint(version));
        self.args.push(ArgData::NewId { id, interface });
        len + 2
    }

    pub(crate) fn demarshal(&mut self, fds: &mut impl FI, info: &mut impl Info) {
        let mut data = &self.source[2..];
        for arg in self.msg_decl.get_args() {
            let arg_len = match arg.typ {
                Type::Int => self.add_int(data),
                Type::Uint => self.add_uint(data),
                Type::Fixed => self.add_fixed(data),
                Type::String => self.add_string(data),
                Type::Object => self.add_object(data),
                Type::NewId => self.add_new_id(data, info),
                Type::Array => self.add_array(data),
                Type::Fd => self.add_fd(fds.try_take_fd().expect("Not enough FDs")),
            };
            data = &data[arg_len..];
        }
        assert!(data.is_empty());
    }

    #[cold]
    pub(crate) fn new(header: MessageHeader, msg_decl: RMessage, data: &'a [u32]) -> Self {
        assert_eq!(header.len32(), data.len());
        #[allow(clippy::default_trait_access)]
        Self {
            msg_decl,
            header,
            args: SmallVec::with_capacity(msg_decl.args.len()),
            source: data,
            maybe_modified: false,
            maybe_arg_type_changed: false,
            fds: SmallVec::with_capacity(msg_decl.num_fds as usize),
        }
    }

    fn test_arg_invariant(&self) {
        assert!(
            self.msg_decl.args.len() == self.args.len()
                || (self.msg_decl.is_wl_registry_bind()
                    && self.msg_decl.args.len() + 2 == self.args.len())
        );
    }

    fn marshal_modified_check_types(self, sender: &mut impl MO) -> IoResult<usize> {
        self.test_arg_invariant();
        sender.send(self.fds, |mut ext| {
            for arg_typ in self.args.iter().zip(self.msg_decl.args.iter().map(|a| a.typ)) {
                match arg_typ {
                    (ArgData::Int(i), Type::Int) => ext.add_i32(*i),
                    (ArgData::Uint(u), Type::Uint) => ext.add_u32(*u),
                    (ArgData::Fixed(f), Type::Fixed) => ext.add_i32(*f),
                    (ArgData::String(s), Type::String) => ext.add_array(s.to_bytes_with_nul()),
                    (ArgData::Object(i), Type::Object) => ext.add_u32(*i),
                    (ArgData::NewId { id, .. }, Type::NewId) => ext.add_u32(*id),
                    (ArgData::Array(a), Type::Array) => ext.add_array(a),
                    (ArgData::Fd { .. }, Type::Fd) => {} // TBD: check index?
                    (arg, typ) => panic!("{arg:?} vs. {typ:?} mismatch"),
                };
            }
            self.header
        })
    }

    fn marshal_modified(self, sender: &mut impl MO) -> IoResult<usize> {
        sender.send(self.fds, |mut ext| {
            for arg in &self.args {
                match arg {
                    ArgData::Int(i) => ext.add_i32(*i),
                    ArgData::Uint(u) => ext.add_u32(*u),
                    ArgData::Fixed(f) => ext.add_i32(*f),
                    ArgData::String(s) => ext.add_array(s.to_bytes_with_nul()),
                    ArgData::Object(i) => ext.add_u32(*i),
                    ArgData::NewId { id, .. } => ext.add_u32(*id),
                    ArgData::Array(a) => ext.add_array(a),
                    ArgData::Fd { .. } => {} // TBD: check index?
                };
            }
            self.header
        })
    }

    pub(crate) fn marshal(self, sender: &mut impl MO) -> IoResult<usize> {
        if self.maybe_arg_type_changed {
            self.marshal_modified_check_types(sender)
        } else if self.maybe_modified {
            self.marshal_modified(sender)
        } else {
            self.relay_unmodified(sender)
        }
    }

    pub(crate) fn relay_unmodified(self, sender: &mut impl MO) -> IoResult<usize> {
        debug_assert!(!self.maybe_modified);
        sender.send_raw(self.fds, self.source)
    }
}

mod debug {
    use super::{ArgData, AsRawFd, DemarshalledMessage, Info, RInterface};

    impl<'a> DemarshalledMessage<'a> {
        pub(crate) fn debug_print(&self, info: &impl Info) {
            let mut first = true;
            for arg in &self.args {
                if first {
                    first = false;
                } else {
                    eprint!(", ");
                }
                match arg {
                    ArgData::Int(i) => eprint!("{i}"),
                    ArgData::Uint(u) => eprint!("{u}"),
                    ArgData::Fixed(f) => debug_print_wayland_fixed(*f),
                    ArgData::String(s) if s.is_empty() => eprint!("nil"),
                    ArgData::String(s) => eprint!("{s:?}"),
                    ArgData::Object(i) => debug_print_object_id(*i, info),
                    ArgData::NewId { id, interface } => debug_print_new_id(*id, interface),
                    ArgData::Array(a) => eprint!("array[{}]", a.len()),
                    ArgData::Fd { index } => eprint!("fd {}", self.fds[*index].as_raw_fd()),
                }
            }
        }
    }

    fn debug_print_wayland_fixed(f: i32) {
        const MOD: i32 = 256;
        const TEN8: i32 = 100_000_000;
        const MAGIC: i32 = TEN8 / MOD;
        if f >= 0 {
            eprint!("{}.{:08}", f / MOD, MAGIC * (f % MOD));
        } else {
            eprint!("-{}.{:08}", f / -MOD, -MAGIC * (f % MOD));
        }
    }

    fn debug_print_object_id(id: u32, info: &impl Info) {
        let name = match info.try_lookup(id) {
            Some(interface) => &interface.name,
            None => "<unknown interface>",
        };
        eprint!("{name}#{id}");
    }

    fn debug_print_new_id(id: u32, interface: RInterface) {
        eprint!("new_id {}${id}", &interface.name);
    }
}

impl<'a> MessageInfo<'a> for DemarshalledMessage<'a> {
    fn get_num_args(&self) -> usize { self.args.len() }

    fn get_arg(&self, index: usize) -> &ArgData<'a> { &self.args[index] }

    fn get_arg_mut(&mut self, index: usize) -> &mut ArgData<'a> {
        self.maybe_modified = true;
        self.maybe_arg_type_changed = true;
        &mut self.args[index]
    }

    fn get_args_mut(&mut self) -> &mut [ArgData<'a>] {
        self.maybe_modified = true;
        self.maybe_arg_type_changed = true;
        self.args.as_mut_slice()
    }

    fn get_decl(&self) -> &'static Message<'static> { self.msg_decl }

    fn get_object_id(&self) -> u32 { self.header.object_id }

    fn set_arg(&mut self, index: usize, a: ArgData<'a>) {
        use std::mem::discriminant as discr;
        let sa = &mut self.args[index];
        // don't allow the discriminant of the arg to be changed, only its data
        assert_eq!(discr(sa), discr(&a), "{sa:?} vs. {a:?} mismatch");
        self.maybe_modified = true;
        *sa = a;
    }

    fn get_size(&self) -> usize { self.header.size as usize }
}
