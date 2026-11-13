// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicU32, Ordering};

use lazy_static::lazy_static;

use crate::common::RWOp;
use crate::util::regmap::RegMap;

use super::PCI_HOTPLUG_BASE_LEN;

#[derive(Copy, Clone, Eq, PartialEq)]
enum Reg {
    PciUp,
    PciDown,
    Eject,
    Bus,
    Reserved,
}
impl Reg {
    const fn name(&self) -> &'static str {
        match self {
            Self::PciUp => "PCIU",
            Self::PciDown => "PCID",
            Self::Eject => "B0EJ",
            Self::Bus => "BNUM",
            Self::Reserved => "RESERVED",
        }
    }
}

lazy_static! {
    static ref REGS: RegMap<Reg> = {
        let layout = [
            (Reg::PciUp, 4),
            (Reg::PciDown, 4),
            (Reg::Eject, 4),
            (Reg::Reserved, 4),
            (Reg::Bus, 4),
            (Reg::Reserved, 4),
        ];
        RegMap::create_packed(PCI_HOTPLUG_BASE_LEN.into(), &layout, None)
    };
}

pub struct PciHotplug {
    pci_up: AtomicU32,
    pci_down: AtomicU32,
    eject: AtomicU32,
    bus: AtomicU32,
}
impl PciHotplug {
    pub fn new() -> Self {
        Self {
            pci_up: 0.into(),
            pci_down: 0.into(),
            eject: 0.into(),
            bus: 0.into(),
        }
    }

    pub fn pio_rw(&self, port: u16, mut rwo: RWOp) {
        REGS.process(&mut rwo, |reg, rwo| {
            let field = match reg {
                Reg::PciUp => &self.pci_up,
                Reg::PciDown => &self.pci_down,
                Reg::Eject => &self.eject,
                Reg::Bus => &self.bus,
                Reg::Reserved => unreachable!("invalid access"),
            };

            match rwo {
                RWOp::Read(ro) => {
                    let bits = match reg {
                        Reg::PciUp | Reg::PciDown => {
                            // PCIU and PCID are clear-on-read.
                            field.swap(0, Ordering::AcqRel)
                        }
                        _ => field.load(Ordering::Acquire),
                    };
                    ro.write_u32(bits);

                    probes::pci_hotplug_io_read!(|| (reg.name(), bits));
                }
                RWOp::Write(wo) => {
                    let bits = wo.read_u32();
                    let old = field.swap(bits, Ordering::AcqRel);

                    probes::pci_hotplug_io_write!(|| (reg.name(), old, bits));
                }
            }
        });
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn pci_hotplug_io_read(reg: &str, value: u32) {}
    fn pci_hotplug_io_write(reg: &str, old: u32, new: u32) {}
}
