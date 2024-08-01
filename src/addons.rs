pub(crate) mod safeclip {
    use std::{borrow::Cow, ffi::CString, sync::OnceLock};

    use crate::for_handlers::{
        AddHandler, ArgData, MessageHandlerResult, MessageInfo, SessionInfo,
    };

    static PREFIX: OnceLock<CString> = OnceLock::new();

    pub(crate) fn init_handler(args: &[String], adder: &mut dyn AddHandler) {
        // we expect exactly one arg, which is the prefix.  Unlike the C waydapt, the 0th arg is NOT
        // the dll name, it is our first arg.
        assert_eq!(args.len(), 1);
        let prefix = &args[0];
        let cprefix = CString::new(prefix.clone()).unwrap();
        PREFIX.set(cprefix).unwrap();
        let requests = [
            ("wl_shell_surface", "set_title"),
            ("xdg_toplevel", "set_title"),
            ("wl_data_offer", "accept"),
            ("wl_data_offer", "receive"),
            ("wl_data_source", "offer"),
            ("zwp_primary_selection_offer_v1", "receive"),
            ("zwp_primary_selection_source_v1", "offer"),
            ("zwlr_data_control_source_v1", "offer"),
            ("zwlr_data_control_offer_v1", "receive"),
        ];
        for (iname, mname) in requests {
            adder.request_push_front(iname, mname, add_prefix).unwrap();
        }
        let events = [
            ("wl_data_offer", "offer"),
            ("wl_data_source", "target"),
            ("wl_data_source", "send"),
            ("zwp_primary_selection_offer_v1", "offer"),
            ("zwp_primary_selection_source_v1", "send"),
            ("zwlr_data_control_source_v1", "send"),
            ("zwlr_data_control_offer_v1", "offer"),
        ];
        for (iname, mname) in events {
            adder.event_push_front(iname, mname, remove_prefix).unwrap();
        }
        // We do not need a session init handler, because there is no per-session state
    }

    // Ideally, we could have a per-thread (per-session) buffer for concatenating where the prefix
    // is already present, and which gets borrowed by the Cow in the ArgData, so that there are no
    // allocations or frees.  A very elaborate alternative would be to have an additional field with
    // the Cow which is a Option closure that, if present, is called on the Cow and the output
    // buffer, and writes to the output buffer.  Actually, not a closure, but an object with at
    // least two methods (trait?): len and output.  The object should be stateless - may be zero
    // sized.  Box<dyn T> is fine for that, right?  Also, the output method would have to be very
    // low-level, like OutBuffer::push_array in terms of directing output to chunks, which would be
    // very difficult.  An alternative is to switch to 2*4K output chunks, so that no message needs
    // to be split over chunks, so can have simple output routines.  Or, alter push_arrray so that
    // it is based on a lower level push_slice, which does not write the length.  We would then also
    // need a push_zero_term.  The object, instead of being a trait, can be a static struct that has
    // two closure or funpointer fields.
    fn add_prefix(msg: &mut dyn MessageInfo, _si: &mut dyn SessionInfo) -> MessageHandlerResult {
        // find the first String arg and add PREFIX to the front of it:
        for i in 0..msg.get_num_args() {
            if let ArgData::String(s) = msg.get_arg(i) {
                let prefix = PREFIX.get().unwrap();
                let both = [prefix.to_bytes(), s.to_bytes()].concat();
                let cstring = CString::new(both).unwrap();
                msg.set_arg(i, ArgData::String(cstring.into()));
                return MessageHandlerResult::Next;
            }
        }
        panic!("Expected a string arg, didn't find one");
    }

    fn prefixed(prefix: &[u8], s: &[u8]) -> bool {
        let plen = prefix.len();
        s.len() >= plen && prefix == &s[..plen]
    }

    fn remove_prefix(msg: &mut dyn MessageInfo, _si: &mut dyn SessionInfo) -> MessageHandlerResult {
        // find the first String arg and remove PREFIX from the front of it:
        let prefix = PREFIX.get().unwrap().to_bytes();
        let plen = prefix.len();
        for i in 0..msg.get_num_args() {
            match msg.get_arg_mut(i) {
                // Almost all cases will be borrowed, which saves us a copy:
                ArgData::String(Cow::Borrowed(s)) if prefixed(prefix, s.to_bytes()) => {
                    *s = &s[plen..]; // O(1) move, no copy
                    return MessageHandlerResult::Next;
                }
                // Not worth optimizing this case - it would only happen if we have a previous handler
                // modifying this same string arg:
                ArgData::String(Cow::Owned(s)) if prefixed(prefix, s.to_bytes()) => {
                    *s = CString::new(&s.to_bytes()[plen..]).unwrap(); // O(N) copy
                    return MessageHandlerResult::Next;
                }
                ArgData::String(_) => return MessageHandlerResult::Drop,
                _ => {}
            }
        }
        panic!("Expected a string arg, didn't find one");
    }
}
