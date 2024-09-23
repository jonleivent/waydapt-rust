#![forbid(unsafe_code)]
#[allow(clippy::wildcard_imports)]
use super::protocol::*;
use crate::basics::{UnwindDo, LEAKER};
use crate::crate_traits::Alloc;
use std::cmp::min;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Result as IoResult, Write};
use std::path::PathBuf;

type ActiveInterfaceMap<'a> = HashMap<&'a str, &'a Interface<'a>>;

#[derive(Debug)]
pub struct ActiveInterfacesA<'a> {
    map: ActiveInterfaceMap<'a>,
    display: &'a Interface<'a>, // wd_display interface must always be active
}

pub type ActiveInterfaces = ActiveInterfacesA<'static>;
type InterfaceMap<'a> = HashMap<&'a str, Vec<&'a Interface<'a>>>;
type AllProtocols<'a> = [&'a Protocol<'a>];

pub(crate) fn active_interfaces(
    protocol_filenames: impl IntoIterator<Item = PathBuf>, globals_filename: &str,
    allow_missing: bool,
) -> &'static ActiveInterfaces {
    static ACTIVE_INTERFACES: OnceLock<&ActiveInterfaces> = OnceLock::new();
    ACTIVE_INTERFACES.get_or_init(|| {
        let alloc = &LEAKER;
        let mut all_protocols: Vec<&'static Protocol<'static>> = Vec::new();
        let mut maybe_display = None; // wl_display must exist
        for ref protocol_filename in protocol_filenames {
            let _ud = UnwindDo(|| eprintln!("In file {}", protocol_filename.as_path().display()));
            let file = File::open(protocol_filename).unwrap();
            let protocol = super::parse::parse(file, alloc);
            if protocol.name == "wayland" {
                assert!(maybe_display.is_none(), "Base wayland protocol seen twice");
                maybe_display = Some(fixup_wayland_get_display(protocol));
            }
            all_protocols.push(protocol);
        }
        let display = maybe_display.expect("Missing base wayland protocol");
        let map = postparse(&all_protocols, globals_filename, allow_missing);
        debug_assert!(display.is_active(), "wl_display is not active");
        alloc.alloc(ActiveInterfacesA { map, display })
    })
}

fn fixup_wayland_get_display<'a>(wayland_protocol: &'a Protocol<'a>) -> &'a Interface<'a> {
    // Mostly this is to mark the special messages in the wayland protocol, but it also needs to
    // handle setting the parent of wl_callback - see note below.
    assert_eq!(wayland_protocol.name, "wayland");
    let registry =
        wayland_protocol.find_interface("wl_registry").expect("Missing interface wl_registry");
    let bind = registry.requests.first().expect("Interface wl_registry has no requests");
    assert_eq!(bind.name, "bind", "Expected wl_registry::bind, got {bind:?}");
    bind.special.set(SpecialMessage::WlRegistryBind).unwrap();

    let global = registry.events.first().expect("Interface wl_registry has no events");
    assert_eq!(global.name, "global", "Expected wl_registry::global, got {global:?}");
    global.special.set(SpecialMessage::WlRegistryGlobal).unwrap();

    let display =
        wayland_protocol.find_interface("wl_display").expect("Missing interface wl_display");
    // Set the display limited version to 1, since it never changes, and isn't allowed in the
    // globals file:
    display.limited_version.set(1).unwrap();
    let sync = display.requests.first().expect("Interface wl_display has no requests");
    assert_eq!(sync.name, "sync", "Expected wl_display::sync, got {sync:?}");
    sync.special.set(SpecialMessage::WlDisplaySync).unwrap();

    let delete_id = display.events.get(1).expect("Interface wl_display has no delete_id event");
    assert_eq!(delete_id.name, "delete_id", "Expected wl_display::delete_id, got {delete_id:?}");
    delete_id.special.set(SpecialMessage::WlDisplayDeleteId).unwrap();

    let callback =
        wayland_protocol.find_interface("wl_callback").expect("Missing interface wl_callback");
    // Because wl_callback violates the single-parent rule (it's the only interface that
    // does), we have to set its parent here to wl_display so that it doesn't pick up a
    // different parent later that is then rendered inactive by the global limits file.
    callback.parent.set(display).expect("Should only be set here");
    display
}

impl<'a> ActiveInterfacesA<'a> {
    #![allow(clippy::must_use_candidate)]
    #![allow(clippy::missing_panics_doc)]
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
        if let Some(p) = iface.parent.get() {
            panic!("{iface} is not global, it has parent {p}")
        }
        iface
    }
    pub fn get_display(&self) -> &'a Interface<'a> { self.display }

    pub fn iter(&self) -> impl Iterator<Item = &Interface<'a>> {
        let i = self.map.values().copied();
        i
    }

    pub(crate) fn dump(&self, out: &mut impl Write) -> IoResult<()> {
        for interface in self.map.values() {
            interface.dump(out)?;
        }
        Ok(())
    }
}

fn postparse<'a>(
    protocols: &AllProtocols<'a>, globals_filename: &str, allow_missing: bool,
) -> ActiveInterfaceMap<'a> {
    // First pass: set the owner backpointers (which cannot be set during parsing without using
    // comprehensive internal mutability), find new_id args on messages and set their interfaces if
    // they're local to the owning protocol, else put them on externals.  Also set parents
    // of new_id interfaces.
    let (externals, parentless) = externals_parentless_set_owners(protocols);

    // Second pass (well, not really a pass - the iteration is over lines in the globals file):
    // determine which protocols and global interfaces are active based on the globals file, which
    // also includes a version limit for each global.
    get_globals_limits(&parentless, globals_filename, protocols, allow_missing);

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
    for protocol @ Protocol { interfaces, .. } in protocols {
        for interface @ Interface { owner, .. } in interfaces.values() {
            owner.set(protocol).expect("should only be set here");
            for message @ Message { owner, new_id_interface, .. } in interface.all_messages() {
                owner.set(interface).expect("should only be set here");
                // Find at-most-one new_id arg with a named interface.  If found, link it to an
                // interface in this protocol (if it exists), else mark the message as external (to
                // be linked in a later pass)
                let Some(target_interface_name) = message.new_id_interface_name() else { continue };
                if let Some(target) = protocol.find_interface(target_interface_name) {
                    new_id_interface.set(target).expect("should only be set here");
                    // the parent of an interface is the only other interface in the same protocol
                    // that creates instances of it.  An interface with no parent can only be
                    // instantiated by wl_registry.bind.
                    target.set_parent(interface);
                } else {
                    // try later to link to an active interface in another protocol:
                    externals.push(message);
                }
            }
        }
        // Gather parentless interfaces as potential globals for use by set_global_limits.  Note
        // that parent_interface is set above, but not on the interface being looped over - so this
        // has to be a separate loop over interfaces.
        for interface in interfaces.values().filter(|i| i.parent.get().is_none()) {
            parentless.entry(&interface.name).or_default().push(interface);
        }
    }
    (externals, parentless)
}

fn get_globals_limits(
    parentless: &InterfaceMap, filename: &str, protocols: &AllProtocols<'_>, allow_missing: bool,
) {
    for (n, ref name, version_limit) in global_limits(filename) {
        match parentless.get(name.as_str()).map(Vec::as_slice) {
            Some([interface @ Interface { parsed_version, limited_version, .. }]) => {
                // a global (or other) interface is considered active if its limited_version is set
                assert!(
                    limited_version.set(min(version_limit, *parsed_version)).is_ok(),
                    "Multiple entries for {interface} in global limits file {filename} line {n}"
                );
                // activate the owning protocol as well.  Expected to be set multiple times, once for
                // each global interface, so don't panic:
                let _ = interface.owning_protocol().active.set(());
            }
            Some([]) => panic!("empty vector element in parentless map"),
            Some(multiple) => panic!("Multiple globals with same name: {}", Foster(multiple)),
            None => {
                if let Some(iface) = protocols.iter().find_map(|p| p.find_interface(name)) {
                    let parent_name = &iface.parent.get().unwrap().name;
                    panic!(
                        "Non-global interface {name} (parent {parent_name}) in global limits file {filename} line {n}"
                    );
                } else if allow_missing {
                    eprintln!("Interface {name} not found: global limits file {filename} line {n}");
                } else {
                    panic!("Interface {name} not found: global limits file {filename} line {n}");
                }
            }
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
    let is = protocols.iter().filter(|p| p.is_active()).flat_map(|p| p.interfaces.values());
    for interface @ Interface { name, limited_version, parsed_version, .. } in is {
        let global_ancestor = interface.global_ancestor();
        let Some(global_limit) = global_ancestor.limited_version.get() else { continue };
        // interface is active because it has an active global ancestor
        let conflict = |i2: &mut _| panic!("Active interface conflict: {i2} vs. {interface}");
        active_interfaces.entry(name).and_modify(conflict).or_insert(interface);
        let limit = min(*parsed_version, *global_limit);
        for message in interface.all_messages().filter(|m| m.since <= limit) {
            message.active.set(()).expect("message.active should only be set here");
        }
        if !interface.same_as(global_ancestor) {
            assert!(
                limited_version.set(limit).is_ok(),
                "non-global {} with global {} limited_version clash",
                interface.name,
                global_ancestor.name
            );
        }
    }
    active_interfaces
}

fn link_externals<'a>(externals: &[&'a Message<'a>], actives: &ActiveInterfaceMap<'a>) {
    for msg in externals.iter().filter(|m| m.is_active()) {
        let name = msg.new_id_interface_name().expect("should exist for external msgs");
        let Some(external_interface) = actives.get(name.as_str()) else {
            panic!("Missing active interface {name} for external {msg}")
        };
        msg.new_id_interface.set(external_interface).expect("should only be linked here");
    }
}

// --------------------------------------------------------------------------------

impl<'a> Interface<'a> {
    fn set_parent(&self, parent: &'a Interface<'a>) {
        // can be called multiple times, but the interfaces have to agree, unless we're dealing with
        // wl_callback, which is the only allowed violator of the single-parent rule.
        if let Err(prev_parent) = self.parent.set(parent) {
            if parent.same_as(prev_parent) {
                return;
            }
            panic!("{self} has at least two parents {} and {}", prev_parent.name, parent.name)
        }
    }

    // only accurate after parent links are set, which is done during the first postparse pass
    fn global_ancestor(&self) -> &Interface<'a> {
        // Don't rely on tail recursion
        let mut p = self;
        while let Some(next_p) = p.parent.get() {
            p = next_p;
        }
        p
    }
}

impl<'a> Message<'a> {
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
        assert!(new_id_args.next().is_none(), "{self} has more than one new_id arg");
        target_interface_name
    }
}

struct GlobalLimits<'a, LI> {
    filename: &'a str,
    line_iter: LI,
}

fn global_limits(filename: &str) -> impl Iterator<Item = (usize, String, u32)> + '_ {
    use std::io::{BufRead, BufReader};
    let file = File::open(filename).unwrap_or_else(|e| panic!("Cannot open {filename}: {e}"));
    let reader = BufReader::new(file);
    let line_iter = (1..).zip(reader.lines());
    GlobalLimits { filename, line_iter }
}

impl<'a, LI> Iterator for GlobalLimits<'a, LI>
where LI: Iterator<Item = (usize, IoResult<String>)>
{
    type Item = (usize, String, u32);

    fn next(&mut self) -> Option<Self::Item> {
        for (n, line) in &mut self.line_iter {
            let filename = self.filename;
            let line = line.unwrap_or_else(|e| panic!("Error in {filename}, line {n}: {e}"));
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else { continue }; // skip blank lines
            if name.starts_with('#') {
                continue;
            }; // skip comments
            let Some(version_field) = fields.next() else {
                panic!(
                    "Missing version limit field for global {name} in global limits file {filename} line {n}"
                )
            };
            let rest: Vec<_> = fields.collect();
            assert!(
                rest.is_empty(),
                "Extraneous fields {rest:?} for global {name} in global limits file {filename} line {n}"
            );
            let Ok(version_limit) = version_field.parse() else {
                panic!(
                    "Malformed version limit field \"{version_field}\" for {name} in global limits file {filename} line {n}"
                )
            };
            // make version_limit == 0 act like unlimited
            let version_limit = if version_limit > 0 { version_limit } else { u32::MAX };
            return Some((n, name.to_string(), version_limit));
        }
        None
    }
}

pub(crate) struct Foster<'a, T: ?Sized>(pub(crate) &'a T);

use std::fmt;
impl<'a, T: fmt::Display> fmt::Display for Foster<'a, [T]> {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let mut first = true;
        write!(f, "[")?;
        for e in self.0 {
            if first {
                first = false;
                write!(f, "{e}")?;
            } else {
                write!(f, ", {e}")?;
            }
        }
        write!(f, "]")
    }
}
