use crate::protocol::Interface;

pub(crate) const WL_SERVER_ID_START: u32 = 0xff00_0000;

type RInterface = &'static Interface<'static>;

#[derive(Clone, Copy, Debug)]
pub enum ObjectEntry {
    Live(RInterface),
    Deleted(RInterface),
    NeverUsed,
}

type ObjectMap = Vec<ObjectEntry>;
// only needs get, get_mut, push, len

#[derive(Debug)]
pub(crate) struct IdMap {
    client_id_map: ObjectMap,
    server_id_map: ObjectMap,
}

impl IdMap {
    fn normalize_mut(&mut self, id: u32) -> (&mut ObjectMap, u32) {
        if id < WL_SERVER_ID_START {
            (&mut self.client_id_map, id)
        } else {
            (&mut self.server_id_map, id - WL_SERVER_ID_START)
        }
    }

    fn normalize(&self, id: u32) -> (&ObjectMap, u32) {
        if id < WL_SERVER_ID_START {
            (&self.client_id_map, id)
        } else {
            (&self.server_id_map, id - WL_SERVER_ID_START)
        }
    }

    pub(crate) fn new() -> Self {
        Self { client_id_map: ObjectMap::new(), server_id_map: ObjectMap::new() }
    }

    pub(crate) fn try_lookup(&self, id: u32) -> Option<RInterface> {
        let (map, id) = self.normalize(id);
        if let Some(ObjectEntry::Live(interface)) = map.get(id as usize) {
            Some(interface)
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn lookup(&self, id: u32) -> RInterface {
        let i = self.try_lookup(id);
        i.unwrap_or_else(|| panic!("No interface for object id {id}"))
    }

    pub(crate) fn add(&mut self, orig_id: u32, interface: RInterface) {
        let (map, id) = self.normalize_mut(orig_id);
        if id as usize == map.len() {
            map.push(ObjectEntry::Live(interface));
        } else {
            let Some(e) = map.get_mut(id as usize) else {
                panic!("Out of range add id: {id}, interface: {interface}")
            };
            if let ObjectEntry::Live(_) = *e {
                // we trust the server to replace live entries without having seen a delete request
                // from the client (because there are none).  If the orig_id is from the server,
                // then it is not the same as the normalized id:
                assert!(orig_id != id, "Duplicate add");
            };
            *e = ObjectEntry::Live(interface);
        }
    }

    pub(crate) fn delete(&mut self, id: u32) {
        let (map, id) = self.normalize_mut(id);
        // This must do nothing (don't panic!) if the id is not currently an object.
        let Some(e) = map.get_mut(id as usize) else { return };
        let ObjectEntry::Live(interface) = e else { return };
        *e = ObjectEntry::Deleted(interface);
    }
}
