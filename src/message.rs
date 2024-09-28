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

            // id_map.delete(id);

            // There's also the return handshake request, which is a sync with a high id that doesn't
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
        assert_eq!(header.msg_nwords(), data.len());
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

#[cfg(test)]
mod test {
    use std::collections::VecDeque;

    use crate::{
        basics::test_util::*, buffers::privates::*, for_handlers::SessionInitInfo,
        postparse::ActiveInterfaces, protocol::Arg,
    };

    use super::*;

    fn fake_msg_decl() -> &'static Message<'static> {
        let msg = Box::leak(Box::new(Message::new(42)));
        msg.name = "fake_msg".into();
        let args = vec![
            Arg { name: "a_int".into(), typ: Type::Int, interface_name: None },
            Arg { name: "a_uint".into(), typ: Type::Uint, interface_name: None },
            Arg { name: "a_fixed".into(), typ: Type::Fixed, interface_name: None },
            Arg { name: "a_string".into(), typ: Type::String, interface_name: None },
            Arg { name: "an_object".into(), typ: Type::Object, interface_name: None },
            Arg { name: "an_array".into(), typ: Type::Array, interface_name: None },
            Arg { name: "an_fd".into(), typ: Type::Fd, interface_name: None },
            Arg { name: "an_fd".into(), typ: Type::Fd, interface_name: None },
            Arg {
                name: "a_new_id".into(),
                typ: Type::NewId,
                interface_name: Some("fake_interface".into()),
            },
        ];
        msg.args = args;
        let iface = Box::leak(Box::new(Interface::new()));
        iface.name = "fake_interface".into();
        msg.new_id_interface.set(iface).unwrap();
        msg
    }

    fn fake_msg_data() -> Vec<u32> {
        let mut data = vec![
            0,
            0,           // room for header later
            0x1234_5678, // a_int
            0x90ab_cdef, // a_uint
            0x1357_9ace, // a_fixed
            27,          //a_string length, including final 0
            u32::from_ne_bytes(*b"abcd"),
            u32::from_ne_bytes(*b"efgh"),
            u32::from_ne_bytes(*b"ijkl"),
            u32::from_ne_bytes(*b"mnop"),
            u32::from_ne_bytes(*b"qrst"),
            u32::from_ne_bytes(*b"uvwx"),
            u32::from_ne_bytes(*b"yz\0\0"),
            9876, // an object id
            10,   // an_array length
            u32::from_ne_bytes(*b"0123"),
            u32::from_ne_bytes(*b"4567"),
            u32::from_ne_bytes(*b"89\0\0"),
            // the fds take up no data
            5432, // a new_id
        ];
        #[allow(clippy::cast_possible_truncation)]
        let hdr =
            MessageHeader { object_id: 13, opcode: 42, size: data.len() as u16 * 4 }.as_words();
        data[0] = hdr[0];
        data[1] = hdr[1];
        data
    }

    fn fake_msg_data_modified() -> Vec<u32> {
        let mut data = vec![
            0,
            0,           // room for header later
            0x5678_1234, // a_int
            0x90ab_cdef, // a_uint
            0x9ace_1357, // a_fixed
            13,          //a_string length, including final 0
            u32::from_ne_bytes(*b"ABCD"),
            u32::from_ne_bytes(*b"IJKL"),
            u32::from_ne_bytes(*b"QRST"),
            0,
            9876, // an object id
            16,   // an_array length
            u32::from_ne_bytes(*b"abcd"),
            u32::from_ne_bytes(*b"0123"),
            u32::from_ne_bytes(*b"4567"),
            u32::from_ne_bytes(*b"890!"),
            // the fds take up no data
            5432, // a new_id
        ];
        #[allow(clippy::cast_possible_truncation)]
        let hdr =
            MessageHeader { object_id: 13, opcode: 42, size: data.len() as u16 * 4 }.as_words();
        data[0] = hdr[0];
        data[1] = hdr[1];
        data
    }

    struct FakeSessionInfo(Vec<(u32, RInterface)>);

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl SessionInitInfo for FakeSessionInfo {
        fn ucred(&self) -> Option<rustix::net::UCred> { None }
        fn get_active_interfaces(&self) -> &'static ActiveInterfaces { todo!() }
        fn get_display(&self) -> RInterface { todo!() } // wl_display
        fn get_debug_level(&self) -> u32 { 0 }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl Info for FakeSessionInfo {
        fn try_lookup(&self, _id: u32) -> Option<RInterface> { None }
        fn lookup(&self, _id: u32) -> RInterface { todo!() }
        fn add(&mut self, id: u32, interface: RInterface) { self.0.push((id, interface)); }
        fn delete(&mut self, _id: u32) {}
    }

    struct FakeMessenger(Chunk, Vec<OwnedFd>);

    impl MO for FakeMessenger {
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

    fn test_msg(msg_data: &mut Vec<u32>) -> DemarshalledMessage<'_> {
        let msg_decl = fake_msg_decl();
        *msg_data = fake_msg_data();
        let hdr = MessageHeader::new(msg_data);
        let mut dm = DemarshalledMessage::new(hdr, msg_decl, msg_data);
        let mut vdq: VecDeque<OwnedFd> = VecDeque::new();
        let mut fsi = FakeSessionInfo(Vec::new());
        vdq.push_back(fd_for_test(1));
        vdq.push_back(fd_for_test(2));
        dm.demarshal(&mut vdq, &mut fsi);

        assert_eq!(dm.get_num_args(), 9);
        matches!(dm.get_arg(0), ArgData::Int(0x1234_5678));
        matches!(dm.get_arg(1), ArgData::Uint(0x90ab_cdef));
        matches!(dm.get_arg(2), ArgData::Fixed(0x1357_9ace));
        let ArgData::String(Cow::Borrowed(s)) = dm.get_arg(3) else { panic!() };
        assert_eq!(s, &c"abcdefghijklmnopqrstuvwxyz");
        matches!(dm.get_arg(4), ArgData::Object(9876));
        let ArgData::Array(Cow::Borrowed(a)) = dm.get_arg(5) else { panic!() };
        assert_eq!(a, b"0123456789");
        matches!(dm.get_arg(6), ArgData::Fd { index: 0 });
        let ArgData::NewId { id: 5432, interface } = dm.get_arg(8) else { panic!() };
        assert_eq!(interface.name, msg_decl.new_id_interface.get().unwrap().name);
        assert_eq!(msg_decl.find_new_id(msg_data), 5432);

        assert_eq!(fsi.0.len(), 1);
        let (id, iface) = fsi.0[0];
        assert_eq!(id, 5432);
        assert_eq!(iface.name, interface.name);
        assert_eq!(dm.fds.len(), 2);
        assert!(check_test_fd(&dm.fds[0], 1));
        assert!(check_test_fd(&dm.fds[1], 2));
        dm
    }

    fn test_demarshal(check_types: bool) {
        let mut msg_data = Vec::new();
        let dm = test_msg(&mut msg_data);
        let mut fm = FakeMessenger(Chunk::new(), Vec::new());
        if check_types {
            assert_eq!(dm.marshal_modified_check_types(&mut fm).unwrap(), msg_data.len());
        } else {
            assert_eq!(dm.marshal_modified(&mut fm).unwrap(), msg_data.len());
        }
        assert_eq!(&msg_data[..], &fm.0[..]);
        assert_eq!(fm.1.len(), 2);
        assert!(check_test_fd(&fm.1[0], 1));
        assert!(check_test_fd(&fm.1[1], 2));
    }

    #[test]
    fn test_demarshal1() { test_demarshal(false); }

    #[test]
    fn test_demarshal2() { test_demarshal(true); }

    #[test]
    fn test_demarshal_modified1() {
        #![allow(clippy::cast_possible_wrap)]
        let mut msg_data = Vec::new();
        let mut dm = test_msg(&mut msg_data);

        *dm.get_arg_mut(0) = ArgData::Int(0x5678_1234);
        *dm.get_arg_mut(2) = ArgData::Fixed(0x9ace_1357_u32 as i32);
        let ArgData::String(ref mut c) = dm.get_arg_mut(3) else { panic!() };
        *c = c"ABCDIJKLQRST".into();
        let ArgData::Array(ref mut a) = dm.get_arg_mut(5) else { panic!() };
        *a = b"abcd01234567890!".into();

        let mod_msg_data = fake_msg_data_modified();
        assert_eq!(dm.msg_decl.find_new_id(&mod_msg_data), 5432);

        let mut fm = FakeMessenger(Chunk::new(), Vec::new());
        assert_eq!(dm.marshal(&mut fm).unwrap(), mod_msg_data.len());
        assert_eq!(&mod_msg_data[..], &fm.0[..]);
        assert_eq!(fm.1.len(), 2);
        assert!(check_test_fd(&fm.1[0], 1));
        assert!(check_test_fd(&fm.1[1], 2));
    }
}
