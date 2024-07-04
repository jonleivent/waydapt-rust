#![warn(clippy::pedantic)]
#![allow(unused)]

use crate::protocol::Interface;

pub(crate) const WL_SERVER_ID_START: u32 = 0xff00_0000;

type RInterface = &'static Interface<'static>;

pub(crate) enum ObjectEntry {
    Live(RInterface),
    Deleted(RInterface),
}

pub(crate) struct WaylandObjectMap<const IS_SERVER_SIDE: bool> {
    vect: Vec<ObjectEntry>,
}

// TBD: switch to using the Peer enum instead of a bool generic param.

impl<const IS_SERVER_SIDE: bool> WaylandObjectMap<IS_SERVER_SIDE> {
    pub(crate) fn new() -> Self {
        Self { vect: Vec::new() }
    }

    fn normalize_id(id: u32) -> usize {
        if IS_SERVER_SIDE {
            assert!(id >= WL_SERVER_ID_START, "Wrong side id");
            (id - WL_SERVER_ID_START) as usize
        } else {
            assert!(id < WL_SERVER_ID_START, "Wrong side id");
            id as usize
        }
    }

    pub(crate) fn lookup(&self, id: u32) -> Option<RInterface> {
        let id = Self::normalize_id(id);
        if let Some(ObjectEntry::Live(interface)) = self.vect.get(id) {
            Some(interface)
        } else {
            None
        }
    }

    pub(crate) fn add(&mut self, id: u32, interface: RInterface) {
        let id = Self::normalize_id(id);
        if id == self.vect.len() {
            self.vect.push(ObjectEntry::Live(interface));
        } else {
            let Some(e) = self.vect.get_mut(id) else { panic!("Out of range add") };
            if let ObjectEntry::Live(_) = *e {
                // we trust the server to replace live entries without having seen a delete request
                // from the client (because there are none):
                assert!(IS_SERVER_SIDE, "Duplicate add");
            };
            *e = ObjectEntry::Live(interface);
        }
    }

    pub(crate) fn delete(&mut self, id: u32) {
        // This must do nothing (don't panic!) if the id is not currently an object.
        let id = Self::normalize_id(id);
        let Some(e) = self.vect.get_mut(id) else { return };
        let ObjectEntry::Live(interface) = e else { return };
        *e = ObjectEntry::Deleted(interface);
    }
}
