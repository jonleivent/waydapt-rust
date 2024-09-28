#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, VecDeque},
    env::Args,
};

use crate::{
    basics::Leaker,
    crate_traits::Alloc,
    for_handlers::{AddHandler, AddHandlerError, MessageHandler, SessionInitHandler},
    postparse::ActiveInterfaces,
    protocol::{Interface, Message},
    setup::IHMap,
};

pub(crate) type SessionHandlers = VecDeque<SessionInitHandler>;

pub(crate) fn gather_handlers(
    all_args: &mut Args, init_handlers: &IHMap, active_interfaces: &'static ActiveInterfaces,
) -> &'static SessionHandlers {
    let mut all_handlers = AllHandlers::new(active_interfaces);

    // add addon session and message handlers based on what's left in all_args iterator
    all_handlers.add_addon_handlers(all_args, init_handlers);

    // By adding the builtin handlers last, we give them the ability to prevent addon handlers (by
    // using push_front and MessageHandlerResult::Send or Drop), or allow them (by using push_back
    // or push_front and Next):
    all_handlers.mod_name = "<builtin>";
    crate::builtin::add_builtin_handlers(&mut all_handlers);

    all_handlers.link_with_messages();
    all_handlers.session_handlers
}

type RInterface = &'static Interface<'static>;
type RMessage = &'static Message<'static>;

type HandlerMap = HashMap<&'static str, (RMessage, VecDeque<(&'static str, MessageHandler)>)>;

#[derive(Default)]
struct InterfaceHandlers {
    request_handlers: HandlerMap,
    event_handlers: HandlerMap,
}

impl InterfaceHandlers {
    const fn map<const IS_REQUEST: bool>(&mut self) -> &mut HandlerMap {
        if IS_REQUEST { &mut self.request_handlers } else { &mut self.event_handlers }
    }
}

struct AllHandlers {
    message_handlers: HashMap<&'static str, (RInterface, InterfaceHandlers)>,
    session_handlers: &'static mut SessionHandlers,
    mod_name: &'static str,
    active_interfaces: &'static ActiveInterfaces,
}

impl AddHandlerError {
    const fn no_such_msg<const IS_REQUEST: bool>() -> Self {
        if IS_REQUEST { Self::NoSuchRequest } else { Self::NoSuchEvent }
    }

    const fn inactive<const IS_REQUEST: bool>() -> Self {
        if IS_REQUEST { Self::InactiveRequest } else { Self::InactiveEvent }
    }
}

impl<'a> Interface<'a> {
    fn get_msg_by_name<const IS_REQUEST: bool>(&self, name: &str) -> Option<&Message<'a>> {
        if IS_REQUEST { self.get_request_by_name(name) } else { self.get_event_by_name(name) }
    }
}

impl AllHandlers {
    #![allow(clippy::default_trait_access)]
    fn new(active_interfaces: &'static ActiveInterfaces) -> Self {
        // The session handlers have to be 'static because they are shared across sessions
        Self {
            message_handlers: Default::default(),
            session_handlers: Leaker.alloc(Default::default()),
            mod_name: "",
            active_interfaces,
        }
    }

    fn add_addon_handlers(&mut self, all_args: &mut Args, init_handlers: &IHMap) {
        // Call init handlers for addon modules in the order that their names appear on the command
        // line so that they can add their addon handlers.  The remainder of all_args will be name
        // args -- name args -- ...., so we need to break it up into portions using take_while.
        let active_interfaces = self.active_interfaces;
        loop {
            let Some(handler_mod_name) = all_args.next() else { break };
            let handler_mod_name = Leaker.alloc(handler_mod_name).as_str();
            let Some(handler_init) = init_handlers.get(handler_mod_name) else {
                panic!("{handler_mod_name} does not have a handler init function");
            };
            // this init handler gets the next sequence of args up to the next --
            let handler_args = all_args.take_while(|a| a != "--").collect::<Vec<_>>();
            self.mod_name = handler_mod_name;
            handler_init(&handler_args, self, active_interfaces);
        }
    }

    fn link_with_messages(&mut self) {
        // Set the Message.handlers fields
        for (_, (_interface, mut interface_handlers)) in self.message_handlers.drain() {
            for (_, (request, request_handlers)) in interface_handlers.request_handlers.drain() {
                request.handlers.set(request_handlers).expect("should only be set once");
            }
            for (_, (event, event_handlers)) in interface_handlers.event_handlers.drain() {
                event.handlers.set(event_handlers).expect("should only be set once");
            }
        }
    }

    // get the handler queue for a specific interface/message by names
    fn get_handlers<const IS_REQUEST: bool>(
        &mut self, iface_name: &'static str, msg_name: &'static str,
    ) -> Result<&mut VecDeque<(&'static str, MessageHandler)>, AddHandlerError> {
        use std::collections::hash_map::Entry;
        let ientry = self.message_handlers.entry(iface_name);
        let (interface, iface_handlers) = match ientry {
            Entry::Vacant(ve) => ve.insert((
                self.active_interfaces
                    .maybe_get_interface(iface_name)
                    .ok_or(AddHandlerError::NoSuchInterface)?,
                Default::default(),
            )),
            Entry::Occupied(oe) => oe.into_mut(),
        };
        let mentry = iface_handlers.map::<IS_REQUEST>().entry(msg_name);
        let (_message, msg_handlers) = match mentry {
            Entry::Vacant(ve) => {
                let message = interface
                    .get_msg_by_name::<IS_REQUEST>(msg_name)
                    .ok_or(AddHandlerError::no_such_msg::<IS_REQUEST>())?;
                message
                    .is_active()
                    .then(|| ve.insert((message, Default::default())))
                    .ok_or(AddHandlerError::inactive::<IS_REQUEST>())?
            }
            Entry::Occupied(oe) => oe.into_mut(),
        };
        Ok(msg_handlers)
    }
}

impl AddHandler for AllHandlers {
    fn request_push_front(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<true>(interface_name, request_name)?;
        handlers.push_front((mod_name, handler));
        Ok(())
    }
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<true>(interface_name, request_name)?;
        handlers.push_back((mod_name, handler));
        Ok(())
    }
    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<false>(interface_name, event_name)?;
        handlers.push_front((mod_name, handler));
        Ok(())
    }
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    ) -> Result<(), AddHandlerError> {
        let mod_name = self.mod_name;
        let handlers = self.get_handlers::<false>(interface_name, event_name)?;
        handlers.push_back((mod_name, handler));
        Ok(())
    }
    fn session_push_front(&mut self, handler: SessionInitHandler) {
        self.session_handlers.push_front(handler);
    }
    fn session_push_back(&mut self, handler: SessionInitHandler) {
        self.session_handlers.push_back(handler);
    }
}
