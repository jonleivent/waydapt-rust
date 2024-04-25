pub use ghost_cell::{GhostCell, GhostToken};

pub struct Protocol<'g, 'a> {
    pub name: String,
    pub interfaces: Vec<&'a Interface<'g, 'a>>,
    pub active: GhostCell<'g, bool>,
}

impl<'g, 'a> Protocol<'g, 'a> {
    pub fn new(name: String) -> Protocol<'g, 'a> {
        Protocol {
            name,
            interfaces: Vec::new(),
            active: GhostCell::new(false),
        }
    }

    pub fn find_interface(&self, name: &str) -> Option<&Interface<'g, 'a>> {
        self.interfaces
            .iter()
            .find(|iface| iface.name == name)
            .copied()
    }
}

pub struct Interface<'g, 'a> {
    pub name: String,
    pub version: u32,
    pub requests: Vec<&'a Message<'g, 'a>>, // indexed by opcode
    pub events: Vec<&'a Message<'g, 'a>>,   // indexed by opcode
    pub limited_version: GhostCell<'g, u32>,
    pub owning_protocol: GhostCell<'g, Option<&'a Protocol<'g, 'a>>>,
    pub parent_interface: GhostCell<'g, Option<&'a Interface<'g, 'a>>>,
}

impl<'g, 'a> Interface<'g, 'a> {
    pub fn new() -> Interface<'g, 'a> {
        Interface {
            name: String::new(),
            version: 1,
            requests: Vec::new(),
            events: Vec::new(),
            limited_version: GhostCell::new(0),
            owning_protocol: GhostCell::new(None),
            parent_interface: GhostCell::new(None),
        }
    }
}

pub struct Message<'g, 'a> {
    pub name: String,
    pub since: u32,
    pub opcode: u32,
    pub args: Vec<Arg>,
    pub owning_interface: GhostCell<'g, Option<&'a Interface<'g, 'a>>>,
    pub new_id_interface: GhostCell<'g, Option<&'a Interface<'g, 'a>>>,
    pub new_id_arg: GhostCell<'g, Option<&'a Arg>>,
    pub active: GhostCell<'g, bool>,
}

impl<'g, 'a> Message<'g, 'a> {
    pub fn new(opcode: u32) -> Message<'g, 'a> {
        Message {
            name: String::new(),
            since: 1,
            opcode,
            args: Vec::new(),
            owning_interface: GhostCell::new(None),
            new_id_interface: GhostCell::new(None),
            new_id_arg: GhostCell::new(None),
            active: GhostCell::new(false),
        }
    }
}

pub struct Arg {
    pub name: String,
    pub typ: Type,
    pub interface: Option<String>,
    pub allow_null: bool,
}

impl Arg {
    pub fn new() -> Arg {
        Arg {
            name: String::new(),
            typ: Type::Object,
            interface: None,
            allow_null: false,
        }
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
