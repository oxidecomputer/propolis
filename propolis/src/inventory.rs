use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Inventory {
    inner: Mutex<InventoryInner>,
}
impl Inventory {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(InventoryInner::default()) }
    }

    pub fn register<T: Entity>(&self, ent: Arc<T>, name: String) -> EntityID {
        let any = Arc::clone(&ent) as Arc<dyn Any + Send + Sync>;
        let entptr = &*ent as *const T as usize;
        let rec = Record { any, ent, name };

        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id();
        inner.entities.insert(id, rec);
        inner.reverse.insert(entptr, id);

        id
    }
    pub fn print(&self) {
        let inner = self.inner.lock().unwrap();
        for (id, rec) in inner.entities.iter() {
            println!(
                "{:x}: {} {:x?}",
                id.num, rec.name, &*rec.any as *const dyn Any
            );
        }
    }
}

#[derive(Default)]
struct InventoryInner {
    entities: BTreeMap<EntityID, Record>,
    reverse: HashMap<usize, EntityID>,
    next_id: usize,
}
impl InventoryInner {
    fn next_id(&mut self) -> EntityID {
        self.next_id += 1;
        EntityID { num: self.next_id }
    }
}

// XXX: still a WIP
#[allow(unused)]
struct Record {
    any: Arc<dyn Any + Send + Sync + 'static>,
    ent: Arc<dyn Entity>,
    name: String,
}

pub trait Entity: Send + Sync + 'static {}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EntityID {
    num: usize,
}
