use crate::for_handlers::InitHandlersFun;

// For now, we require that all addons add a pair element to this:
pub const ALL_ADDONS: &[(&str, InitHandlersFun)] = &[("safeclip", safeclip::init_handlers)];

/// Example addon: `SafeClip`
///
/// A simple addon that provides more security for Wayland clipboard operations involving sandboxed
/// applications.
///
/// Wayland's clipboard is already more secure than Xorg's because any particular application can
/// only see the clipboard contents when one of its windows is focused.  However, it is easy for the
/// user to click on the wrong window while navigating between apps, potentially giving that
/// window's app access to secrets in the clipboard contents.  Also, some Wayland compositors grant
/// any newly created window focus, allowing any app to steal focus and access the clipboard.
///
/// This can be especially problematic when some apps are not trusted, for instance, those running
/// within sandboxes.
///
/// If a sandbox is configured so that applications within it see only the socket created by waydapt
/// with this addon (and not the compositor's socket), then the windows of those applications will
/// be clearly marked (the title will have a prefix, assuming server-side decorations), and the
/// clipboard those applications share will act as though it is separated from the clipboard used
/// "outside" the sandbox.  Multiple sandboxes can either have their clipboards tied together (give
/// them the same prefix, or use the same waydapt socket) or separated (different prefixes and
/// different waydapt instances).
///
/// With a little bit of external tooling (I use a small script combined with wl-clipboard), one can
/// transfer the clipboard contents into, out of, or between sandboxes in a safer way than relying
/// on window focus.
///
/// It works by altering the "mime type" of Wayland clipboard messages to produce a filtering
/// behavior.  Clipboard requests (client -> server messages) that pass through a waydapt+Safeclip
/// with a prefix `|Foo|` (for example) will have `|Foo|` prepended to their mime types.  More
/// importantly, clipboard events (server -> client) will be required to have `|Foo|` as the prefix
/// of their mime types, else they will be dropped before any client served by that waydapt sees
/// them.  Those events that do have the right mime type prefix will have that prefix removed by the
/// waydapt and then get passed to clients of that waydapt.  The result is that clients served by
/// waydapt+SafeClip using the same prefix will act as though they share a clipboard, but they won't
/// have any access to the clipboard contents of external clients or clients of waydapt+SafeClip
/// using a different prefix.  External clients (not using waydapt+SafeClip) can see all of the
/// clipboards (when they get focus), but will tend to ignore any of the prefixed mime types.
///
/// Even if the added security of the `SafeClip` isn't that interesting to you, it does illustrate
/// how a waydapt addon can create a useful feature by a slight alteration of some of the Wayland
/// message traffic between its clients and the server.
mod safeclip {
    use std::{
        borrow::Cow,
        ffi::{CStr, CString},
    };

    use crate::for_handlers::{
        ActiveInterfaces, AddHandler, ArgData, MAX_BYTES_OUT, Message, MessageHandlerResult,
        MessageInfo, SessionInfo, SessionInitHandler, SessionInitInfo, SessionState, Type,
    };

    #[inline]
    fn has_mime_type_arg(msg: &Message<'_>) -> bool {
        msg.args.iter().any(|a| a.typ == Type::String && a.name == "mime_type")
    }

    // Just to make sure the protocol files aren't altered so that these known cases no longer work:
    fn check_known_mime_type_msgs(active_interfaces: &'static ActiveInterfaces) {
        let mime_type_requests = [
            ("wl_data_offer", "accept"),
            ("wl_data_offer", "receive"),
            ("wl_data_source", "offer"),
            ("zwp_primary_selection_offer_v1", "receive"),
            ("zwp_primary_selection_source_v1", "offer"),
            ("zwlr_data_control_source_v1", "offer"),
            ("zwlr_data_control_offer_v1", "receive"),
        ];
        for (iname, rname) in mime_type_requests {
            if let Some(interface) = active_interfaces.maybe_get_interface(iname) {
                if let Some(request) = interface.get_request_by_name(rname) {
                    assert!(has_mime_type_arg(request), "{request} missing String mime_type arg");
                }
            }
        }
        let mime_type_events = [
            ("wl_data_offer", "offer"),
            ("wl_data_source", "target"),
            ("wl_data_source", "send"),
            ("zwp_primary_selection_offer_v1", "offer"),
            ("zwp_primary_selection_source_v1", "send"),
            ("zwlr_data_control_source_v1", "send"),
            ("zwlr_data_control_offer_v1", "offer"),
        ];
        for (iname, ename) in mime_type_events {
            if let Some(interface) = active_interfaces.maybe_get_interface(iname) {
                if let Some(event) = interface.get_event_by_name(ename) {
                    assert!(has_mime_type_arg(event), "{event} missing String mime_type arg");
                }
            }
        }
    }

    struct SessionInit(&'static String);

    impl SessionInitHandler for SessionInit {
        fn init(&self, _: &dyn SessionInitInfo) -> Box<SessionState> {
            let prefix = self.0;
            Box::new(prefix.clone())
        }
    }

    pub(super) fn init_handlers(
        args: &[String], adder: &mut dyn AddHandler, active_interfaces: &'static ActiveInterfaces,
    ) -> &'static dyn SessionInitHandler {
        // Unlike the C waydapt, the 0th arg is NOT the dll name, it is our first arg.
        assert_eq!(args.len(), 1);
        let prefix = Box::leak(Box::new(args[0].clone()));

        check_known_mime_type_msgs(active_interfaces);

        for iface in active_interfaces.iter() {
            let iname = &iface.name.as_str();
            // Add the add_prefix handler to any request named "set_title" or any that has a
            // mime_type arg
            for &request in &iface.requests {
                if !request.is_active() {
                    continue;
                };
                if request.name == "set_title" || has_mime_type_arg(request) {
                    adder.request_push_back(iname, &request.name, &add_prefix).unwrap();
                }
            }
            // Add the remove_prefix handler to any event that has a mime_type arg
            for &event in &iface.events {
                if !event.is_active() {
                    continue;
                };
                if has_mime_type_arg(event) {
                    adder.event_push_front(iname, &event.name, &remove_prefix).unwrap();
                }
            }
        }

        let f = Box::leak(Box::new(SessionInit(prefix)));
        f
    }

    fn add_prefix(
        msg: &mut dyn MessageInfo, _: &mut dyn SessionInfo, state: &mut SessionState,
    ) -> MessageHandlerResult {
        let prefix: &String = state.downcast_ref::<String>().unwrap();
        add_prefix_internal(msg, prefix.as_bytes())
    }

    fn add_prefix_internal(msg: &mut dyn MessageInfo, prefix: &[u8]) -> MessageHandlerResult {
        // find the first String arg and add prefix to the front of it:
        let msg_size = msg.get_size();
        let msg_decl = msg.get_decl();
        for (i, arg) in msg_decl.args.iter().enumerate() {
            if let ArgData::String(s) = msg.get_arg(i) {
                if arg.name == "mime_type" || arg.name == "title" {
                    let mut sb = s.to_bytes();
                    let newlen = msg_size + prefix.len();
                    if newlen > MAX_BYTES_OUT {
                        // The msg would be too long with the prefix added, so truncate the suffix.
                        // This is probably a set_title request, so such truncation is not a big
                        // deal.  If this is a mime-type message, then the truncation might be a
                        // problem, but mime-type strings are very unlikely to be long enough to
                        // cause a problem.  If one gets truncated, it will likely just result in a
                        // copy-paste mismatch, disallowing the copy.
                        let over = newlen - MAX_BYTES_OUT;
                        let trunc = sb.len() - over;
                        sb = &sb[..trunc];
                    }
                    let fixed = CString::new([prefix, sb].concat()).unwrap();
                    msg.set_arg(i, ArgData::String(fixed.into()));
                    return MessageHandlerResult::Next;
                }
            }
        }
        unreachable!("Didn't find string arg named 'mime_type' or 'title'");
    }

    fn prefixed(prefix: &[u8], s: &CStr) -> bool {
        let plen = prefix.len();
        let s = s.to_bytes_with_nul();
        s.len() >= plen && prefix == &s[..plen]
    }

    fn remove_prefix(
        msg: &mut dyn MessageInfo, _: &mut dyn SessionInfo, state: &mut SessionState,
    ) -> MessageHandlerResult {
        let prefix: &String = state.downcast_ref::<String>().unwrap();
        remove_prefix_internal(msg, prefix.as_bytes())
    }

    fn remove_prefix_internal(msg: &mut dyn MessageInfo, prefix: &[u8]) -> MessageHandlerResult {
        // find the first String arg and remove prefix from the front of it:
        let plen = prefix.len();
        let msg_decl = msg.get_decl();
        for (i, arg) in msg_decl.args.iter().enumerate() {
            if arg.name != "mime_type" {
                continue;
            }
            match msg.get_arg_mut(i) {
                // Almost all cases will be borrowed, which saves us a copy:
                ArgData::String(Cow::Borrowed(s)) if prefixed(prefix, s) => {
                    *s = &s[plen..]; // O(1) move, no copy
                    return MessageHandlerResult::Next;
                }
                // Not worth optimizing this case - it would only happen if we have a previous
                // handler modifying this same string arg:
                ArgData::String(Cow::Owned(s)) if prefixed(prefix, s) => {
                    *s = s[..][plen..].into(); // O(N) copy
                    return MessageHandlerResult::Next;
                }
                ArgData::String(_) => return MessageHandlerResult::Drop,
                _ => {}
            }
        }
        unreachable!("Didn't find string arg named 'mime_type'");
    }

    #[cfg(test)]
    mod test {
        use crate::{
            basics::MAX_WORDS_OUT,
            buffers::privates::Chunk,
            for_handlers::{
                MessageHandlerResult, MessageInfo, RInterface, SessionInfo, SessionInitInfo,
            },
            header::MessageHeader,
            message::{ArgData, DemarshalledMessage},
            postparse::ActiveInterfaces,
            test_utils::*,
        };

        use super::*;
        // TBD: Test how add_prefix works when the msg would become too long with the added prefix.
        // Use a msg with a mime_type or title string arg and another arg that is a very long array.

        struct FakeSessionInfo(Vec<(u32, RInterface)>);

        #[cfg_attr(coverage_nightly, coverage(off))]
        impl SessionInitInfo for FakeSessionInfo {
            fn ucred(&self) -> Option<rustix::net::UCred> { None }
            fn get_active_interfaces(&self) -> &'static ActiveInterfaces { todo!() }
            fn get_display(&self) -> RInterface { todo!() } // wl_display
            fn get_debug_level(&self) -> u32 { 0 }
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        impl SessionInfo for FakeSessionInfo {
            fn try_lookup(&self, _id: u32) -> Option<RInterface> { None }
            fn lookup(&self, _id: u32) -> RInterface { todo!() }
            fn add(&mut self, id: u32, interface: RInterface) { self.0.push((id, interface)); }
            fn delete(&mut self, _id: u32) {}
        }

        #[test]
        fn test_add_prefix_overflow1() {
            let mut data = Chunk::new();
            let (msg_decl, mut fds) = {
                let mut fmm = FakeMsgMaker::new(42, 13);
                fmm.add_string("title", c"test string".to_bytes_with_nul());

                // an array arg that maxes out the msg size:
                let left = MAX_WORDS_OUT - 2 - 4 - 1; // -2=hdr, -4=s, -1=arraylen
                let a: Vec<u8> = vec![0u8; left * 4];
                fmm.add_array("a", &a);
                fmm.produce(&mut data)
            };
            assert_eq!(data.len(), MAX_WORDS_OUT);

            let hdr = MessageHeader::new(&data);
            let mut msg = DemarshalledMessage::new(hdr, msg_decl, &data);
            let mut fsi = FakeSessionInfo(Vec::new());
            msg.demarshal(&mut fds, &mut fsi);
            let ArgData::String(s) = msg.get_arg(0) else { panic!() };
            assert_eq!(s.as_ref(), c"test string");
            let prefix = "pre".as_bytes();
            assert_eq!(add_prefix_internal(&mut msg, prefix), MessageHandlerResult::Next);
            let ArgData::String(s) = msg.get_arg(0) else { panic!() };

            // add_prefix should add the "pre" prefix, but truncate the "test string" as a result:
            assert_eq!(s.as_ref(), c"pretest str");
        }

        #[test]
        fn test_add_prefix_overflow2() {
            let mut data = Chunk::new();
            let (msg_decl, mut fds) = {
                let mut fmm = FakeMsgMaker::new(42, 13);
                fmm.add_string("title", c"test string".to_bytes_with_nul());

                // an array arg that does not quite max out the msg size
                let left = MAX_WORDS_OUT - 2 - 4 - 1 - 1; // -2=hdr, -4=s, -1=arraylen, -1 slack
                let a: Vec<u8> = vec![0u8; left * 4];
                fmm.add_array("a", &a);
                fmm.produce(&mut data)
            };
            assert_eq!(data.len(), MAX_WORDS_OUT - 1);

            let hdr = MessageHeader::new(&data);
            let mut msg = DemarshalledMessage::new(hdr, msg_decl, &data);
            let mut fsi = FakeSessionInfo(Vec::new());
            msg.demarshal(&mut fds, &mut fsi);
            let ArgData::String(s) = msg.get_arg(0) else { panic!() };
            assert_eq!(s.as_ref(), c"test string");
            let prefix = "prefix".as_bytes();
            assert_eq!(add_prefix_internal(&mut msg, prefix), MessageHandlerResult::Next);
            let ArgData::String(s) = msg.get_arg(0) else { panic!() };

            // add_prefix should add the "pre" prefix, but truncate the "test string" as a result:
            assert_eq!(s.as_ref(), c"prefixtest stri"); // 15 chars + 0 term is all that will fit

            let mut fm = FakeMessenger(Chunk::new(), Vec::new());
            assert_eq!(msg.marshal(&mut fm).unwrap(), MAX_WORDS_OUT);
        }
    }
}

// TBD: maybe add another addon module that is just for testing.  This would allow us to test
// session init handlers, which safeclip doesn't use.

// Do we really need session init handlers?  Each addon could have a thread_local static LazyCell.
// The issue would be that in multi-thread mode, one thread might block the other while initing the
// LazyCell.  Also, it might be useful for certain addons to do some session initialization before
// any messages are sent, especially when we add origination.
