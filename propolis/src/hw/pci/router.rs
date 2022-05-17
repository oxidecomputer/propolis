//! Finds routes to specific buses in a PCI topology.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{Bus, BusNum};

#[derive(Default)]
pub struct Router {
    inner: Mutex<Inner>,
}

impl Router {
    /// Gets the bus currently assigned with the supplied bus number.
    ///
    /// Note: The supplied bus number's routing may change before this function
    /// returns.
    pub fn get(&self, n: BusNum) -> Option<Arc<Bus>> {
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
    pub fn set(&self, n: BusNum, bus: Option<Arc<Bus>>) {
        self.inner.lock().unwrap().set(n, bus)
    }
}

#[derive(Default)]
struct Inner {
    map: BTreeMap<BusNum, Arc<Bus>>,
}

impl Inner {
    fn get(&self, n: BusNum) -> Option<Arc<Bus>> {
        if let Some(bus) = self.map.get(&n) {
            Some(bus.clone())
        } else {
            None
        }
    }

    fn set(&mut self, n: BusNum, bus: Option<Arc<Bus>>) {
        if let Some(bus) = bus {
            let old = self.map.insert(n, bus);
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
