// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! USB Emulation

pub mod xhci;

pub mod usbdev;

#[cfg(test)]
mod test {
    use crate::{hw::pci, vmm::PhysMap};
    use std::sync::Arc;

    /// Helper for tests needing a stand-in for the [PciXhci]'s [DeviceState].
    /// Creates a faux PCI device with 16KiB of RAM, which aught to be enough
    /// for anyunit.
    pub(crate) fn pci_state_for_test() -> Arc<pci::DeviceState> {
        let mut pci_state = pci::Builder::new(pci::Ident::default())
            .add_cap_msix(pci::BarN::BAR0, 1)
            .finish();
        let mut phys_map = PhysMap::new_test(16 * 1024);
        phys_map.add_test_mem("guest-ram".to_string(), 0, 16 * 1024).unwrap();
        pci_state.acc_mem = phys_map.finalize();
        Arc::new(pci_state)
    }
}
