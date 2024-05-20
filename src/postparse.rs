use super::protocol::*;
use bumpalo::Bump;
use std::collections::HashMap;
use std::fs::File;
use std::{cmp::min, ptr::eq};

type ActiveInterfaceMap<'a> = HashMap<&'a str, &'a Interface<'a>>;

struct ActiveInterfacesA<'a> {
    map: ActiveInterfaceMap<'a>,
    display: &'a Interface<'a>, // wd_display interface must always be active
}

pub type ActiveInterfaces = ActiveInterfacesA<'static>;
type InterfaceMap<'a> = HashMap<&'a str, Vec<&'a Interface<'a>>>;
type AllProtocols<'a> = [&'a Protocol<'a>];

pub fn active_interfaces<'a>(
    protocol_filenames: impl Iterator<Item = &'a str>, globals_filename: &str,
) -> &'static ActiveInterfaces {
    static ACTIVE_INTERFACES: OnceLock<&ActiveInterfaces> = OnceLock::new();
    ACTIVE_INTERFACES.get_or_init(|| {
        let bump = Box::leak(Box::new(Bump::new()));
        let mut all_protocols: Vec<&'static Protocol<'static>> = Vec::new();
        let mut maybe_display = None;
        for protocol_filename in protocol_filenames {
            let file = File::open(protocol_filename)
                .unwrap_or_else(|e| panic!("Can't open {protocol_filename} : {e}"));
            let protocol = super::parse::parse(file, bump);
            if protocol.name == "wayland" {
                maybe_display.is_none() || panic!("Base wayland protocol seen twice");
                maybe_display = protocol.find_interface("wl_display");
                let Some(display) = maybe_display else {
                    panic!("Missing interface wl_display");
                };
                // Because wl_callback violates the single-parent rule (it's the only interface that
                // does), we have to set its parent here to wl_display so that it doesn't pick up a
                // different parent later that is then rendered inactive by the global limits file.
                let Some(callback) = protocol.find_interface("wl_callback") else {
                    panic!("Missing interface wl_callback")
                };
                callback.parent.set(display).expect("Should only be set here");
            }
            all_protocols.push(protocol);
        }
        let Some(display) = maybe_display else {
            panic!("Missing base wayland protocol");
        };
        let map = postparse(&all_protocols, globals_filename);
        display.is_active() || panic!("wl_display is not active");
        bump.alloc(ActiveInterfacesA { map, display })
    })
}

impl<'a> ActiveInterfacesA<'a> {
    pub fn maybe_get_interface(&self, name: &str) -> Option<&'a Interface<'a>> {
        self.map.get(name).copied()
    }
    pub fn get_interface(&self, name: &str) -> &'a Interface<'a> {
        self.maybe_get_interface(name).unwrap_or_else(|| panic!("No active interface named {name}"))
    }
    pub fn maybe_get_global(&self, name: &str) -> Option<&'a Interface<'a>> {
        self.map.get(name).filter(|i| i.parent.get().is_none()).copied()
    }
    pub fn get_global(&self, name: &str) -> &'a Interface<'a> {
        let iface = self.get_interface(name);
        iface.parent.get().map(|p| panic!("{iface} is not global, it has parent {p}"));
        iface
    }
    pub fn get_display(&self) -> &'a Interface<'a> {
        self.display
    }
}

fn postparse<'a>(protocols: &AllProtocols<'a>, globals_filename: &str) -> ActiveInterfaceMap<'a> {
    // First pass: set the owner backpointers (which cannot be set during parsing without using
    // comprehensive internal mutability), find new_id args on messages and set their interfaces if
    // they're local to the owning protocol, else put them on externals.  Also set parents
    // of new_id interfaces.
    let (externals, parentless) = externals_parentless_set_owners(protocols);

    // Second pass (well, not really a pass - the iteration is over lines in the globals file):
    // determine which protocols and global interfaces are active based on the globals file, which
    // also includes a version limit for each global.
    get_globals_limits(&parentless, globals_filename);

    // Third pass: propagate version limits to non-globals in the same protocol according to the
    // rules in: https://wayland.freedesktop.org/docs/html/ch04.html#sect-Protocol-Versioning. This
    // determines which messages are active.  Determine active_interfaces.
    let active_interfaces = propagate_limits_find_actives(protocols);

    // Forth pass: link external messages, which can only be done after determining which interfaces
    // are active because otherwise there may be ambiguity (similarly named interfaces in different
    // protocols).
    link_externals(&externals, &active_interfaces);

    active_interfaces
}

type ExternalsParentless<'a> = (Vec<&'a Message<'a>>, InterfaceMap<'a>);
fn externals_parentless_set_owners<'a>(protocols: &AllProtocols<'a>) -> ExternalsParentless<'a> {
    let mut externals = Vec::new(); // should only be a few
    let mut parentless: InterfaceMap = HashMap::new(); // potential globals
    for protocol @ Protocol { interfaces, .. } in protocols.iter() {
        for interface @ Interface { owner, .. } in interfaces.iter() {
            owner.set(protocol).expect("should only be set here");
            for message @ Message { owner, new_id_interface, .. } in interface.all_messages() {
                owner.set(interface).expect("should only be set here");
                // Find at-most-one new_id arg with a named interface.  If found, link it to an
                // interface in this protocol (if it exists), else mark the message as external (to
                // be linked in a later pass)
                let Some(target_interface_name) = message.new_id_interface_name() else { continue };
                if let Some(target) = protocol.find_interface(target_interface_name) {
                    new_id_interface.set(target).expect("should only be set here");
                    // Internal linking is bidirectional via the parent on the target interface:
                    target.set_parent(interface);
                } else {
                    externals.push(message)
                }
            }
        }
        // Gather parentless interfaces as potential globals for use by set_global_limits.  Note
        // that parent_interface is set above, but not on the interface being looped over - so this
        // has to be a separate loop over interfaces.
        for interface in interfaces.iter().filter(|i| i.parent.get().is_none()) {
            parentless.entry(&interface.name).or_insert_with(Vec::new).push(interface);
        }
    }
    (externals, parentless)
}

fn get_globals_limits(parentless: &InterfaceMap, filename: &str) {
    for (n, name, version_limit) in GlobalLimits::iter(filename) {
        match parentless.get(name.as_str()).map(|v| v.as_slice()) {
            Some([interface @ Interface { parsed_version, limited_version, .. }]) => {
                // a global (or other) interface is considered active if its limited_version is set
                limited_version.set(min(version_limit, *parsed_version)).is_ok() ||
                    panic!("Multiple entries for {interface} in global limits file {filename} line {n}");
                // activate the owning protocol as well.  Expected to be set multiple times, once for
                // each global interface, so don't panic:
                let _ = interface.owning_protocol().active.set(());
            }
            None => panic!("Interface {name} not found: global limits file {filename} line {n}"),
            Some([]) => panic!("empty vector element in parentless map"),
            Some(multiple) => panic!("Multiple globals with same name: {}", Foster(multiple)),
        }
    }
}

// We changed this so that not all interfaces in an active protocol are active - only those that
// have an active global ancestor.  The reasoning is that without an active global ancestor, there's
// no way to create an object with that interface from within the current protocol.  But what about
// external messages?  Should we include them in the determination of which interfaces are active?
// But in that case, what's the limited version to use?  There won't be one, as version numbers
// don't propagate across external links.
fn propagate_limits_find_actives<'a>(protocols: &AllProtocols<'a>) -> ActiveInterfaceMap<'a> {
    let mut active_interfaces: ActiveInterfaceMap = HashMap::new();
    for protocol in protocols.iter().filter(|p| p.is_active()) {
        for interface @ Interface { name, .. } in protocol.interfaces.iter() {
            // limit versions by the ancestor rule, and determine which are active:
            if let Some(lv) = interface.limit_version_per_global_ancestor() {
                let conflict = |i: &mut _| panic!("Active interface conflict: {i} vs. {interface}");
                active_interfaces.entry(name).and_modify(conflict).or_insert(interface);
                // Only messages with since <= limited_version are allowed to be active:
                for message in interface.all_messages().filter(|&m| m.since <= lv) {
                    message.active.set(()).expect("message.active should only be set here")
                }
            }
        }
    }
    active_interfaces
}

fn link_externals<'a>(externals: &Vec<&'a Message<'a>>, actives: &ActiveInterfaceMap<'a>) {
    for msg in externals.iter().filter(|m| m.is_active()) {
        let name = msg.new_id_interface_name().expect("should exist for external msgs");
        let Some(external_interface) = actives.get(name.as_str()) else {
            panic!("Missing active interface {name} for external {msg}")
        };
        msg.new_id_interface.set(external_interface).expect("should only be linked here");
    }
}

////

impl<'a> Interface<'a> {
    fn set_parent(&self, parent: &'a Interface<'a>) {
        // can be called multiple times, but the interfaces have to agree, unless we're dealing with
        // wl_callback, which is the only allowed violator of the single-parent rule.
        if let Err(prev_parent) = self.parent.set(parent) {
            eq(prev_parent, parent) && return;
            self.name == "wl_callback" && self.owning_protocol().name == "wayland" && return;
            panic!("{self} has at least two parents {} and {}", prev_parent.name, parent.name)
        }
    }

    // only accurate after parent links are set, which is done during the first postparse pass
    fn global_ancestor(&self) -> &Interface<'a> {
        // Don't rely on tail recursion
        let mut p = self;
        while let Some(next_p) = p.parent.get() {
            p = next_p
        }
        p
    }

    fn limit_version_per_global_ancestor(&self) -> Option<u32> {
        // limit versions by the ancestor rule, and determine which messages are active:
        let global_ancestor = self.global_ancestor();
        // active globals have their limited_version set.  If the global ancestor is not active, then
        // the interface can't be active, so skip it.
        let Some(&global_limit) = global_ancestor.limited_version.get() else { return None };
        let lv = min(self.parsed_version, global_limit);
        self.limited_version.set(lv).is_ok() && { return Some(lv) };
        eq(global_ancestor, self) && { return Some(lv) };
        panic!("non-global interface.limited_version should only be set here")
    }
}

impl<'a> Message<'a> {
    fn is_wl_registry_bind(&self) -> bool {
        let owning_interface = self.owner.get().expect("owning_interface should be set");
        let owning_protocol = owning_interface.owner.get().expect("owning_protocol should be set");
        self.name == "bind"
            && owning_interface.name == "wl_registry"
            && owning_protocol.name == "wayland"
    }

    fn new_id_interface_name(&self) -> Option<&String> {
        let mut new_id_args = self.args.iter().filter(|a| a.typ == Type::NewId);
        let targetless_ok = |name| self.is_wl_registry_bind() && name == "id";
        let bad = |name| panic!("{self} has new_id arg {name} but no interface name");
        let target_interface_name = match new_id_args.next() {
            None => None,
            Some(Arg { interface_name: Some(name), .. }) => Some(name),
            Some(Arg { name, .. }) if targetless_ok(name) => None,
            Some(Arg { name, .. }) => bad(name),
        };
        new_id_args.next().is_some() && panic!("{self} has more than one new_id arg");
        target_interface_name
    }
}

use std::iter::{Enumerate, Iterator};
use std::{io::BufRead, io::BufReader, io::Lines};

struct GlobalLimits<'a> {
    filename: &'a str,
    line_iter: Enumerate<Lines<BufReader<File>>>,
}

impl<'a> GlobalLimits<'a> {
    fn iter(filename: &'a str) -> Self {
        let file = File::open(filename).unwrap_or_else(|e| panic!("Cannot open {filename}: {e}"));
        let reader = BufReader::new(file);
        Self { filename, line_iter: reader.lines().enumerate() }
    }
}

impl<'a> Iterator for GlobalLimits<'a> {
    type Item = (usize, String, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((n, line)) = self.line_iter.next() {
            let n = n + 1;
            let filename = self.filename;
            let line = line.unwrap_or_else(|e| panic!("Error in {filename}, line {n}: {e}"));
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else { return self.next() }; // skip blank lines
            name.starts_with('#') && { return self.next() }; // skip comments
            let Some(version_field) = fields.next() else {
                panic!("Missing version limit field for global {name} in global limits file {filename} line {n}")
            };
            let rest: Vec<_> = fields.collect();
            if rest.len() > 0 {
                panic!("Extraneous fields {rest:?} for global {name} in global limits file {filename} line {n}")
            }
            let Ok(version_limit) = version_field.parse() else {
                panic!("Malformed version limit field \"{version_field}\" for {name} in global limits file {filename} line {n}")
            };
            // make version_limit == 0 act like unlimited
            let version_limit = if version_limit > 0 { version_limit } else { u32::MAX };
            Some((n, name.to_string(), version_limit))
        } else {
            None
        }
    }
}

pub(crate) struct Foster<'a, T: ?Sized>(pub(crate) &'a T);

use std::fmt;
impl<'a, T: fmt::Display> fmt::Display for Foster<'a, [T]> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let mut first = true;
        write!(f, "[")?;
        for e in self.0.iter() {
            if first {
                first = false;
                write!(f, "{}", e)?;
            } else {
                write!(f, ", {}", e)?;
            }
        }
        write!(f, "]")
    }
}
