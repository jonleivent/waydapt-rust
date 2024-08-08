#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use crate::for_handlers::{AddHandler, ArgData, MessageHandlerResult, MessageInfo, SessionInfo};

// We must alter wl_registry::global events in order to either drop or version-limit the global
// objects based on protocol that this waydapt can understand (dictated by the globals file).  We
// also cannot allow any addons to alter or drop the resulting message, so we use Send or Drop
// results only.
fn wl_registry_global_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // arg0 is the "name" of the global instance, really a uint
    let ArgData::String(interface_name) = msg.get_arg(1) else { unreachable!() };
    let interface_name = interface_name.to_str().unwrap();
    let ArgData::Uint(advertised_version) = msg.get_arg(2) else { unreachable!() };
    let active_interfaces = session_info.get_active_interfaces();
    let Some(global_interface) = active_interfaces.maybe_get_global(interface_name) else {
        return MessageHandlerResult::Drop;
    };
    let limited_version = *global_interface.version().unwrap();
    // Do we use limited_version == 0 like the C version does for denoting an interface that was not
    // in the globals file?  Regardless, 0 means drop:
    if limited_version == 0 {
        return MessageHandlerResult::Drop;
    };
    if *advertised_version > limited_version {
        msg.set_arg(2, ArgData::Uint(limited_version));
    }
    // Don't allow any addon handlers to muck up the works:
    MessageHandlerResult::Send
}

fn wl_display_delete_id_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    let ArgData::Object(id) = msg.get_arg(0) else { unreachable!() };
    // should we check if this is the wayland-idfix handshake initial message from the server?  If
    // it is, it has !0 as a server object id, which will never exist, so delete of it will do
    // nothing (it won't panic either).
    session_info.delete(*id);
    // Next or Send works the same here, because these builtin handlers are added last, and we do a
    // push_back for this one - so there can't ever be any addon handlers after it
    MessageHandlerResult::Next
}

// We must handle wl_registry::bind for two reasons: force it to be demarshalled so that its
// uniquely weird new_id arg can be processed properly, and prevent any addon handlers because the
// handler arg access won't work properly vs. the weird new_id arg.
fn wl_registry_bind_handler(
    _msg: &mut dyn MessageInfo, _session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // Nothing to do because it all got handled in the add_wl_registry_bind_new_id method.  However,
    // we don't want any addon handlers to muck up the works, so Send:
    MessageHandlerResult::Send
}

// This will be called after all addon handlers have been added, hence we get priority (push_front)
// or always run last (push_back):
pub(crate) fn add_builtin_handlers(adder: &mut dyn AddHandler) {
    // The wl_registry builtins must override any addon handlers, so put them at the front:
    adder.event_push_front("wl_registry", "global", wl_registry_global_handler).unwrap();
    adder.request_push_front("wl_registry", "bind", wl_registry_bind_handler).unwrap();
    // wl_display::delete_id builtin can run last after any addon handlers:
    adder.event_push_back("wl_display", "delete_id", wl_display_delete_id_handler).unwrap();
}
