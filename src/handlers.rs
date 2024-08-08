#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

use std::{
    collections::{HashMap, VecDeque},
    env::Args,
};

use crate::{
    addons::IHMap,
    basics::Leaker,
    crate_traits::Alloc,
    for_handlers::{AddHandler, AddHandlerError, MessageHandler, SessionInitHandler},
    postparse::ActiveInterfaces,
    protocol::{Interface, Message},
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

struct AllHandlers {
    message_handlers: HashMap<&'static str, (RInterface, InterfaceHandlers)>,
    session_handlers: &'static mut SessionHandlers,
    mod_name: &'static str,
    active_interfaces: &'static ActiveInterfaces,
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
                if request.handlers.set(request_handlers).is_err() {
                    unreachable!("should only be set once");
                }
            }
            for (_, (event, event_handlers)) in interface_handlers.event_handlers.drain() {
                if event.handlers.set(event_handlers).is_err() {
                    unreachable!("should only be set once");
                }
            }
        }
    }

    // get the handler queue for a specific interface/message by names
    fn get_handlers<const IS_REQUEST: bool>(
        &mut self, interface_name: &'static str, msg_name: &'static str,
    ) -> Result<&mut VecDeque<(&'static str, MessageHandler)>, AddHandlerError> {
        use std::collections::hash_map::Entry;
        let entry = self.message_handlers.entry(interface_name);
        let (interface, iface_handlers) = match entry {
            Entry::Vacant(ve) => {
                if let Some(interface) = self.active_interfaces.maybe_get_interface(interface_name)
                {
                    ve.insert((interface, Default::default()))
                } else {
                    return Err(AddHandlerError::NoSuchInterface);
                }
            }
            Entry::Occupied(oe) => oe.into_mut(),
        };
        let entry = if IS_REQUEST {
            iface_handlers.request_handlers.entry(msg_name)
        } else {
            iface_handlers.event_handlers.entry(msg_name)
        };
        let (_message, msg_handlers) = match entry {
            Entry::Vacant(ve) => {
                let (maybe_message, inactive_err, no_such_err) = if IS_REQUEST {
                    (
                        interface.get_request_by_name(msg_name),
                        AddHandlerError::InactiveRequest,
                        AddHandlerError::NoSuchRequest,
                    )
                } else {
                    (
                        interface.get_event_by_name(msg_name),
                        AddHandlerError::InactiveEvent,
                        AddHandlerError::NoSuchEvent,
                    )
                };
                if let Some(message) = maybe_message {
                    if message.is_active() {
                        ve.insert((message, Default::default()))
                    } else {
                        return Err(inactive_err);
                    }
                } else {
                    return Err(no_such_err);
                }
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
