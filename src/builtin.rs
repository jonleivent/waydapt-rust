#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use crate::for_handlers::{AddHandler, ArgData, MessageHandlerResult, MessageInfo, SessionInfo};

fn wl_registry_global_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // arg0 is the "name" of the global instance, really a uint
    let ArgData::String(interface_name) = msg.get_arg(1) else { panic!() };
    let interface_name = interface_name.to_str().unwrap();
    let ArgData::Uint(advertised_version) = msg.get_arg(2) else { panic!() };
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
    MessageHandlerResult::Send
}

fn wl_display_delete_id_handler(
    msg: &mut dyn MessageInfo, session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    let ArgData::Object(id) = msg.get_arg(0) else { panic!() };
    // should we check if this is the wayland-idfix handshake initial message from the server?  If
    // it is, it has !0 as a server object id, which will never exist, so delete of it will do
    // nothing (it won't panic either).
    session_info.delete(*id);
    MessageHandlerResult::Send
}

fn wl_registry_bind_handler(
    _msg: &mut dyn MessageInfo, _session_info: &mut dyn SessionInfo,
) -> MessageHandlerResult {
    // everything got handled in the add_wl_registry_bind_new_id method.  The only reason this
    // exists is to have a handler to force the handler path of Mediator::mediate
    MessageHandlerResult::Send
}

pub(crate) fn add_builtin_handlers(adder: &mut dyn AddHandler) {
    adder.event_push_front("wl_registry", "global", wl_registry_global_handler).unwrap();
    adder.event_push_front("wl_display", "delete_id", wl_display_delete_id_handler).unwrap();
    adder.request_push_front("wl_registry", "bind", wl_registry_bind_handler).unwrap();
}
