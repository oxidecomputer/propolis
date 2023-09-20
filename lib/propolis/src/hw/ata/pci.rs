// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::block;
use crate::hw::pci;
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::accessors::MemAccessor;


pub struct PciAtaController {
    /// PCI device state
    pci_state: Arc<pci::DeviceState>,
    devices: [[Arc<AtaBlockDevice>; 2]; 2],
}

pub struct AtaBlockDevice {
    info: block::DeviceInfo,
    notifier: block::Notifier,
    pci_state: Arc<pci::DeviceState>,
}

struct CompletionPayload {
    channel_id: u8,
    device_id: u8,
}

impl block::Device for AtaBlockDevice {
    fn next(&self) -> Option<block::Request> {
        self.notifier.next_arming(|| None)
    }

    fn complete(&self, op: block::Operation, result: block::Result, payload: Box<block::BlockPayload>) {}

    fn accessor_mem(&self) -> MemAccessor {
        self.pci_state.acc_mem.child()
    }

    fn set_notifier(&self, val: Option<Box<block::NotifierFn>>) {
        self.notifier.set(val);
    }
}

impl pci::Device for PciAtaController {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn bar_rw(&self, bar: pci::BarN, mut _rwo: RWOp) {
        println!("{:?}", bar); //, rwo);
    }
}
