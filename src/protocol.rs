#![forbid(unsafe_code)]
#![allow(clippy::missing_panics_doc)]

use crate::basics::NoDebug;
use crate::handlers::MsgHandlerQ;
use std::collections::HashMap;
use std::fmt;
use std::io::{Result as IoResult, Write};
pub use std::sync::OnceLock;

#[derive(Debug)]
pub struct Protocol<'a> {
    pub name: String,
    pub interfaces: HashMap<&'a str, &'a Interface<'a>>,
    pub(crate) active: OnceLock<()>,
}

impl<'a> Protocol<'a> {
    pub(crate) fn new(name: String) -> Protocol<'a> {
        Protocol { name, interfaces: HashMap::new(), active: OnceLock::new() }
    }

    pub fn find_interface(&self, name: &str) -> Option<&Interface<'a>> {
        self.interfaces.get(name).copied()
    }

    pub fn is_active(&self) -> bool { self.active.get().is_some() }
}

impl fmt::Display for Protocol<'_> {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "protocol {}", self.name)
    }
}

#[derive(Debug)]
pub struct Interface<'a> {
    pub name: String,
    pub(crate) parsed_version: u32,
    pub requests: Vec<&'a Message<'a>>, // indexed by opcode
    pub events: Vec<&'a Message<'a>>,   // indexed by opcode
    pub(crate) limited_version: OnceLock<u32>,
    pub(crate) owner: OnceLock<&'a Protocol<'a>>,
    pub(crate) parent: OnceLock<&'a Interface<'a>>,
}

// TBD: It is the case that, after postparse, the requests and events vectors should have all active
// msgs before all inactive msgs.  But taking advantage of this fact would require making those
// vectors have interior mutability so that we could truncate them.

impl fmt::Display for Interface<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        if let Some(&owning_protocol) = self.owner.get() {
            write!(f, "interface {} in {}", self.name, owning_protocol)
        } else {
            write!(f, "interface {} in <unknown protocol>", self.name)
        }
    }
}

impl<'a> Interface<'a> {
    pub(crate) fn new() -> Interface<'a> {
        Interface {
            name: String::new(),
            parsed_version: 1,
            requests: Vec::new(),
            events: Vec::new(),
            limited_version: OnceLock::new(),
            owner: OnceLock::new(),
            parent: OnceLock::new(),
        }
    }

    pub fn version(&self) -> Option<&u32> { self.limited_version.get() }

    // owning fields are set during postparse first pass
    pub fn owning_protocol(&self) -> &Protocol<'a> {
        self.owner.get().expect("should have been set in postparse")
    }

    pub fn all_messages(&self) -> impl Iterator<Item = &Message<'a>> {
        self.requests.iter().chain(self.events.iter()).copied()
    }

    // TBD: mabye have these two panic with a suitable message if the opcode is out of range:
    pub fn get_request(&self, opcode: usize) -> &Message<'a> { self.requests[opcode] }

    pub fn get_event(&self, opcode: usize) -> &Message<'a> { self.events[opcode] }

    pub(crate) fn get_request_by_name(&self, name: &str) -> Option<&Message<'a>> {
        self.requests.iter().find(|m| m.name == name).copied()
    }

    pub(crate) fn get_event_by_name(&self, name: &str) -> Option<&Message<'a>> {
        self.events.iter().find(|m| m.name == name).copied()
    }

    pub fn get_message(&self, from_server: bool, opcode: usize) -> &Message<'a> {
        if from_server { self.get_event(opcode) } else { self.get_request(opcode) }
    }

    // an interface is considered activated if it had its limited_version set during the third pass of postparse
    pub fn is_active(&self) -> bool { self.limited_version.get().is_some() }

    pub fn same_as(&self, other: &Interface<'a>) -> bool { std::ptr::eq(self, other) }

    pub(crate) fn dump(&self, out: &mut impl Write) -> IoResult<()> {
        let parent = if let Some(parent_interface) = self.parent.get() {
            &parent_interface.name
        } else {
            "<global>"
        };
        writeln!(
            out,
            "{}[v:{}](^:{parent})[in:{}]",
            self.name,
            self.version().unwrap(),
            self.owning_protocol().name
        )?;
        writeln!(out, " requests:")?;
        for r in &self.requests {
            if !r.is_active() {
                break;
            }
            r.dump(out)?;
        }
        writeln!(out, " events:")?;
        for e in &self.events {
            if !e.is_active() {
                break;
            }
            e.dump(out)?;
        }
        writeln!(out)
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum SpecialMessage {
    WlRegistryGlobal,
    WlRegistryBind,
    WlDisplaySync,
    WlDisplayDeleteId,
}

#[derive(Debug)]
pub struct Message<'a> {
    pub name: String,
    pub since: u32,
    pub opcode: u32,
    pub args: Vec<Arg>,
    pub(crate) owner: OnceLock<&'a Interface<'a>>,
    pub(crate) new_id_interface: OnceLock<&'a Interface<'a>>,
    pub(crate) active: OnceLock<()>, // acts like an atomic bool that can only go from inactive -> active
    pub(crate) handlers: NoDebug<OnceLock<MsgHandlerQ>>, // str is module name
    pub num_fds: u32,
    pub is_request: bool,
    pub(crate) special: OnceLock<SpecialMessage>,
}

impl<'a> Message<'a> {
    pub(crate) fn new(opcode: u32) -> Message<'a> {
        Message {
            name: String::new(),
            since: 1,
            opcode,
            args: Vec::new(),
            owner: OnceLock::new(),
            new_id_interface: OnceLock::new(),
            active: OnceLock::new(),
            handlers: NoDebug(OnceLock::new()),
            num_fds: 0,
            is_request: false,
            special: OnceLock::new(),
        }
    }

    pub fn get_name(&self) -> &str { &self.name }

    pub fn is_active(&self) -> bool { self.active.get().is_some() }

    pub fn get_args(&self) -> &[Arg] { &self.args }

    pub fn get_new_id_interface(&self) -> Option<&Interface<'a>> {
        self.new_id_interface.get().copied()
    }
    #[inline]
    pub(crate) fn is_wl_registry_bind(&self) -> bool {
        matches!(self.special.get(), Some(SpecialMessage::WlRegistryBind))
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn is_wl_registry_global(&self) -> bool {
        matches!(self.special.get(), Some(SpecialMessage::WlRegistryGlobal))
    }

    #[inline]
    pub(crate) fn is_wl_display_sync(&self) -> bool {
        matches!(self.special.get(), Some(SpecialMessage::WlDisplaySync))
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn is_wl_display_delete_id(&self) -> bool {
        matches!(self.special.get(), Some(SpecialMessage::WlDisplayDeleteId))
    }

    pub(crate) fn dump(&self, out: &mut impl Write) -> IoResult<()> {
        let kind = if self.is_request { "request" } else { "event" };
        write!(out, "{kind:>10} {}[o:{},s:{}](", self.name, self.opcode, self.since)?;
        for arg in &self.args {
            let typchar = match arg.typ {
                Type::Int => 'i',
                Type::Uint => 'u',
                Type::Fixed => 'f',
                Type::String => 's',
                Type::Object => 'o',
                Type::NewId => 'n',
                Type::Array => 'a',
                Type::Fd => 'h',
            };
            write!(out, "{typchar}")?;
        }
        write!(out, ")")?;
        if let Some(nidi) = self.new_id_interface.get() {
            write!(out, "<n:{}>", nidi.name)?;
        }
        if let Some(handlers) = self.handlers.get() {
            write!(out, "h:[")?;
            let mut first = true;
            for (_, group) in handlers {
                if first {
                    first = false;
                } else {
                    write!(out, ", ")?;
                };
                write!(out, "{group}")?;
            }
            write!(out, "]")?;
        }
        writeln!(out)
    }
}

impl fmt::Display for Message<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        if let Some(&owning_interface) = self.owner.get() {
            write!(f, "message {} in {}", self.name, owning_interface)
        } else {
            write!(f, "message {} in <unknown interface>", self.name)
        }
    }
}

#[derive(Debug)]
pub struct Arg {
    pub name: String,
    pub typ: Type,
    pub interface_name: Option<String>,
}

impl Arg {
    pub(crate) fn new() -> Arg {
        Arg { name: String::new(), typ: Type::Object, interface_name: None }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Type {
    Int,
    Uint,
    Fixed,
    String,
    Object,
    NewId,
    Array,
    Fd,
}
