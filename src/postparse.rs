use super::protocol::*;
use std::collections::HashMap;
use std::ptr::eq;

// Determine if the message has a new_id arg, make sure it is unique,
// and link it to its named interface in the current protocol, and
// make the message's owning interface the parent of the new_id
// interface.  If there is no interface with the required name in the
// current protocol, push the message on the external_messages vector.
//
// the lifetime on the &mut ref to the ghost token is elided and is
// neither 'a nor 'g.  If it is put in explicitly as a third lifetime,
// rust_analyzer will say it can be elided.
fn link_new_id_arg<'g, 'a>(
    g: &mut GhostToken<'g>,
    message: &'a Message<'g, 'a>,
    owning_interface: &'a Interface<'g, 'a>,
    owning_protocol: &'a Protocol<'g, 'a>,
    external_messages: &mut Vec<&Message<'g, 'a>>,
) {
    let mut found: Option<&Arg> = None;
    for arg in message.args.iter() {
        if arg.typ == Type::NewId {
            if let Some(prev_arg) = found {
                panic!(
                    "Message {} of interface {} has more than one new_id arg: {} and {}",
                    message.name, owning_interface.name, prev_arg.name, arg.name
                )
            }
            found = Some(arg);
        }
    }
    if let Some(arg) = found {
        *message.new_id_arg.borrow_mut(g) = found;
        // There is a message that has a new_id arg but no interface - the interface is a string arg
        if let Some(ref new_interface_name) = arg.interface {
            if let Some(new_id_interface) = owning_protocol.find_interface(new_interface_name) {
                *message.new_id_interface.borrow_mut(g) = Some(new_id_interface);
                if let Some(new_id_parent) = *new_id_interface.parent_interface.borrow(g) {
                    if !eq(new_id_parent, owning_interface)
                        && new_id_interface.name != "wl_callback"
                    {
                        // wl_callback is a known violator of single-parent rule
                        panic!(
                            "Interface {} has at least two parent interfaces {} and {} in {}",
                            new_id_interface.name,
                            new_id_parent.name,
                            owning_interface.name,
                            owning_protocol.name
                        )
                    }
                } else {
                    *new_id_interface.parent_interface.borrow_mut(g) = Some(owning_interface);
                }
            } else {
                // new_interface_name not part of the owning protocol -> message is external:
                external_messages.push(message);
            }
        }
    }
}

fn process_global_limit<'g, 'a>(
    g: &mut GhostToken<'g>,
    interfaces: &'a HashMap<&'a str, Vec<&'a Interface<'g, 'a>>>,
    global_name: &str,
    version_limit: u32,
) -> Option<&'a Interface<'g, 'a>> {
    let mut found: Option<&Interface> = None;
    if let Some(v) = interfaces.get(global_name) {
        for &interface in v.iter() {
            if interface.parent_interface.borrow(g).is_some() {
                // can't be global if it has a parent
                continue;
            }
            let protocol = interface.owning_protocol.borrow(g).unwrap();
            if let Some(other) = found {
                panic!(
                    "Global interface {} appears in protocols {} and {}",
                    global_name,
                    other.owning_protocol.borrow(g).unwrap().name,
                    protocol.name
                );
            }
            found = Some(interface);

            if version_limit > 0 && version_limit < interface.version {
                *interface.limited_version.borrow_mut(g) = version_limit;
            } else {
                *interface.limited_version.borrow_mut(g) = interface.version;
            }
            *protocol.active.borrow_mut(g) = true;
        }
    }
    found
}

fn process_global_limits_file<'g, 'a>(
    g: &mut GhostToken<'g>,
    interfaces: &'a HashMap<&'a str, Vec<&'a Interface<'g, 'a>>>,
    globals: &'a mut HashMap<&'a str, &'a Interface<'g, 'a>>,
    filename: &str,
) {
    use std::fs::read_to_string;
    for line in read_to_string(filename)
        .unwrap_or_else(|e| panic!("Cannot read from global limits file {}: {}", filename, e))
        .lines()
    {
        let mut v = line.split(' ');
        let name = v
            .next()
            .unwrap_or_else(|| panic!("Missing name field in global limits file {}", filename));
        let version_limit = v
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "Missing version limit field for {} in global limits file {}",
                    name, filename
                )
            })
            .parse()
            .unwrap_or_else(|e| {
                panic!(
                    "Malformed version limit field for {} in global limits file {}: {}",
                    name, filename, e
                )
            });

        if let Some(interface) = process_global_limit(g, interfaces, name, version_limit) {
            if globals.insert(interface.name.as_str(), interface).is_some() {
                panic!(
                    "Global interface name {} appears more than once in global limits file {}",
                    name, filename
                );
            }
        } else {
            panic!(
                "Global interface {} in global limits file {} not found among protocols",
                name, filename
            )
        }
    }
}

fn interface_global_ancestor<'g, 'a>(
    g: &GhostToken<'g>,
    interface: &'a Interface<'g, 'a>,
) -> &'a Interface<'g, 'a> {
    if let Some(parent_interface) = *interface.parent_interface.borrow(g) {
        interface_global_ancestor(g, parent_interface)
    } else {
        interface
    }
}

fn propagate_version_limits<'g, 'a>(g: &mut GhostToken<'g>, interface: &'a Interface<'g, 'a>) {
    let ancestor = interface_global_ancestor(g, interface);
    let ancestor_limit = *ancestor.limited_version.borrow(g);
    let limited_version = if interface.version > ancestor_limit {
        ancestor_limit
    } else {
        interface.version
    };
    *interface.limited_version.borrow_mut(g) = limited_version;
    for &message in interface.requests.iter().chain(interface.events.iter()) {
        *message.active.borrow_mut(g) = message.since <= limited_version;
    }
}

fn link_external_interfaces<'g, 'a>(
    g: &mut GhostToken<'g>,
    active_interfaces: &'a HashMap<&'a str, &'a Interface<'g, 'a>>,
    external_messages: &[&Message<'g, 'a>],
) {
    for &message in external_messages.iter() {
        if *message.active.borrow(g) {
            let new_id_arg = message.new_id_arg.borrow(g).unwrap_or_else(|| {
                panic!(
                    "External message {}.{} has no new_id arg",
                    message.owning_interface.borrow(g).unwrap().name,
                    message.name,
                )
            });
            let new_id_interface_name = new_id_arg
                .interface
                .as_ref()
                .unwrap_or_else(|| {
                    panic!(
                        "Missing interface name for new_id arg {} for {}.{}",
                        message.owning_interface.borrow(g).unwrap().name,
                        message.name,
                        new_id_arg.name
                    )
                })
                .as_str();
            let external_interface =
                *active_interfaces
                    .get(new_id_interface_name)
                    .unwrap_or_else(|| {
                        panic!(
                            "No active interface {} for external message {}.{}",
                            new_id_interface_name,
                            message.owning_interface.borrow(g).unwrap().name,
                            message.name,
                        )
                    });
            if let Some(ei2) = message
                .new_id_interface
                .replace(Some(external_interface), g)
            {
                panic!(
                    "Duplicate active interfaces for {} ({} and {}) used in new_id arg of {}.{}",
                    new_id_interface_name,
                    external_interface.owning_protocol.borrow(g).unwrap().name,
                    ei2.owning_protocol.borrow(g).unwrap().name,
                    message.owning_interface.borrow(g).unwrap().name,
                    message.name,
                )
            }
        }
    }
}

pub fn postprocess_protocols<'g, 'a>(
    g: &mut GhostToken<'g>,
    interfaces: &'a HashMap<&'a str, Vec<&'a Interface<'g, 'a>>>,
    protocols: &[&'a Protocol<'g, 'a>],
    globals_filename: &str,
    globals: &'a mut HashMap<&'a str, &'a Interface<'g, 'a>>,
    active_interfaces: &'a mut HashMap<&'a str, &'a Interface<'g, 'a>>,
) {
    let mut external_messages: Vec<&Message<'g, 'a>> = Vec::new();

    for &protocol in protocols.iter() {
        for &interface in protocol.interfaces.iter() {
            *interface.owning_protocol.borrow_mut(g) = Some(protocol);
            for &message in interface.events.iter().chain(interface.requests.iter()) {
                *message.owning_interface.borrow_mut(g) = Some(interface);
                link_new_id_arg(g, message, interface, protocol, &mut external_messages);
            }
        }
    }

    process_global_limits_file(g, interfaces, globals, globals_filename);

    // This cannot be moved to process_global_limit because
    // propagate_version_limits requires that all limited_versions are
    // set first (I think):
    for &protocol in protocols.iter() {
        if !protocol.active.borrow(g) {
            continue;
        }
        for &interface in protocol.interfaces.iter() {
            propagate_version_limits(g, interface);
            active_interfaces.insert(interface.name.as_str(), interface);
        }
    }
    link_external_interfaces(g, active_interfaces, &external_messages);
}

pub fn get_display_interface<'g, 'a>(
    active_interfaces: &'a mut HashMap<&'a str, &'a Interface<'g, 'a>>,
) -> &'a Interface<'g, 'a> {
    if let Some(&display_interface) = active_interfaces.get("wd_display") {
        display_interface
    } else {
        panic!("No wl_display interface exists in the active protocol");
    }
}
