pub trait MessageData {}
pub trait SessionInfo {
    fn ucred(&self) -> rustix::net::UCred;
}

pub type MessageHandler = fn(&mut dyn MessageData, &dyn SessionInfo);

pub type SessionInitHandler = fn(&dyn SessionInfo);

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
