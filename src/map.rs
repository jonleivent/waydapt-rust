#![allow(unused)]

use crate::crate_traits::{ClientPeer, Peer, ServerPeer};
use crate::protocol::Interface;
use std::fmt::{Debug, Error, Formatter};
use std::marker::PhantomData;

pub(crate) const WL_SERVER_ID_START: u32 = 0xff00_0000;

type RInterface = &'static Interface<'static>;

#[derive(Clone, Copy, Debug)]
pub enum ObjectEntry {
    Live(RInterface),
    Deleted(RInterface),
    NeverUsed,
}

#[derive(Debug)]
pub(crate) struct ObjectMap<P: Peer> {
    vect: Vec<ObjectEntry>,
    _pd: PhantomData<P>,
}

impl<P: Peer> ObjectMap<P> {
    pub(crate) fn new() -> Self {
        let mut s = Self { vect: Vec::new(), _pd: PhantomData };
        if !P::IS_SERVER {
            // The client never uses id==0, so occupy it:
            s.vect.push(ObjectEntry::NeverUsed);
        }
        s
    }

    #[inline]
    pub(crate) fn try_lookup(&self, id: u32) -> Option<ObjectEntry> {
        let id = P::normalize_id(id);
        self.vect.get(id).copied()
    }

    #[inline]
    pub(crate) fn lookup(&self, id: u32) -> Option<RInterface> {
        let id = P::normalize_id(id);
        if let Some(ObjectEntry::Live(interface)) = self.vect.get(id) {
            Some(interface)
        } else {
            None
        }
    }

    pub(crate) fn add(&mut self, id: u32, interface: RInterface) {
        let id = P::normalize_id(id);
        if id == self.vect.len() {
            self.vect.push(ObjectEntry::Live(interface));
        } else {
            let Some(e) = self.vect.get_mut(id) else {
                panic!("Out of range add id: {id}, interface: {interface}")
            };
            if let ObjectEntry::Live(_) = *e {
                // we trust the server to replace live entries without having seen a delete request
                // from the client (because there are none):
                assert!(P::IS_SERVER, "Duplicate add");
            };
            *e = ObjectEntry::Live(interface);
        }
    }

    pub(crate) fn delete(&mut self, id: u32) {
        // This must do nothing (don't panic!) if the id is not currently an object.
        let id = P::normalize_id(id);
        let Some(e) = self.vect.get_mut(id) else { return };
        let ObjectEntry::Live(interface) = e else { return };
        *e = ObjectEntry::Deleted(interface);
    }
}
