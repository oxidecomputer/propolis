//! Finds routes to specific buses in a PCI topology.

use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::sync::{Arc, Mutex, Weak};

use super::Bus;

/// A simple routing table mapping the secondary bus numbers of PCI bridges to
/// those bridges' downstream [`Bus`]es.
#[derive(Default)]
pub struct Router {
    inner: Mutex<Inner>,
}

impl Router {
    /// Gets the bus currently assigned with the supplied bus number, or None if
    /// no bus is receiving traffic at this number.
    ///
    /// Note: The supplied bus number's routing may change before this function
    /// returns.
    pub fn get(&self, n: NonZeroU8) -> Option<Arc<Bus>> {
        self.inner.lock().unwrap().get(n)
    }

    /// Sets or clears the assigned bus for the supplied bus number.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - The supplied bus is None and the selected routing entry is already
    ///   cleared.
    /// - The supplied bus is Some and the selected routine entry is already
    ///   set.
    pub fn set(&self, n: NonZeroU8, bus: Option<Arc<Bus>>) {
        self.inner.lock().unwrap().set(n, bus)
    }
}

#[derive(Default)]
struct Inner {
    // This reference is weak to avoid a router -> bus -> bridge -> router
    // circular reference chain. This can occur if a PCI bridge is attached
    // to a bus that is itself the downstream bus of another PCI bridge.
    map: BTreeMap<NonZeroU8, Weak<Bus>>,
}

impl Inner {
    fn get(&self, n: NonZeroU8) -> Option<Arc<Bus>> {
        self.map.get(&n).and_then(|bus| bus.upgrade())
    }

    fn set(&mut self, n: NonZeroU8, bus: Option<Arc<Bus>>) {
        if let Some(bus) = bus {
            let old = self.map.insert(n, Arc::downgrade(&bus));
            assert!(
                old.is_none(),
                "Bus already present when routing to {}",
                n.get()
            );
        } else {
            let old = self.map.remove(&n);
            assert!(old.is_some());
        }
    }
}
