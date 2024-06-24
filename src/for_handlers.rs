#![warn(clippy::pedantic)]

pub trait MessageData {}
pub trait SessionInitInfo {
    fn ucred(&self) -> rustix::net::UCred;
    // TBD: get method for active interfaces, etc.
}

// TBD: Somehow, we have to provide access to the object id maps, which will be in the InputHandler,
// not the SessionInitInfo.

pub type MessageHandler = fn(&mut dyn MessageData, &dyn SessionInitInfo);

pub type SessionInitHandler = fn(&dyn SessionInitInfo);

pub trait AddHandler {
    fn request_push_front(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    );
    fn request_push_back(
        &mut self, interface_name: &'static str, request_name: &'static str,
        handler: MessageHandler,
    );

    fn event_push_front(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    );
    fn event_push_back(
        &mut self, interface_name: &'static str, event_name: &'static str, handler: MessageHandler,
    );

    fn session_push_front(&mut self, handler: SessionInitHandler);
    fn session_push_back(&mut self, handler: SessionInitHandler);
}

pub type InitHandlersFun = fn(&[String], &mut dyn AddHandler);
