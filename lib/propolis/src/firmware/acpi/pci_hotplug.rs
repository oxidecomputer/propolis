// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

use lazy_static::lazy_static;

use crate::common::{RWOp, ReadOp, WriteOp};
use crate::hw::pci::Bdf;
use crate::pio::{PioBus, PioFn};
use crate::util::regmap::RegMap;

use super::{
    ssdt_pci_hotplug::{
        B0EJ_LEN, BNUM_LEN, PCID_LEN, PCIU_LEN, PCI_HOTPLUG_IO_ADDR,
        PCI_HOTPLUG_IO_LEN,
    },
    EjectFn, Error,
};

#[derive(Copy, Clone, Eq, PartialEq)]
enum Register {
    PciUp,
    PciDown,
    Eject,
    BusNumber,
}
impl Register {
    const fn name(&self) -> &'static str {
        match self {
            Self::PciUp => "PCIU",
            Self::PciDown => "PCID",
            Self::Eject => "B0EJ",
            Self::BusNumber => "BNUM",
        }
    }
}

lazy_static! {
    static ref REGS: RegMap<Register> = {
        let layout = [
            (Register::PciUp, PCIU_LEN),
            (Register::PciDown, PCID_LEN),
            (Register::Eject, B0EJ_LEN),
            (Register::BusNumber, BNUM_LEN),
        ];
        RegMap::create_packed(PCI_HOTPLUG_IO_LEN.into(), &layout, None)
    };
}

pub struct PciHotplug {
    pci_up: BTreeMap<u8, AtomicU32>,
    pci_down: BTreeMap<u8, AtomicU32>,
    eject: BTreeMap<u8, AtomicU32>,
    bus: AtomicU32,
    pending_ejects: Mutex<BTreeMap<Bdf, EjectFn>>,

    log: slog::Logger,
}
impl PciHotplug {
    pub fn new(log: slog::Logger) -> Self {
        Self {
            pci_up: BTreeMap::from([(0, 0.into())]),
            pci_down: BTreeMap::from([(0, 0.into())]),
            eject: BTreeMap::from([(0, 0.into())]),
            bus: 0.into(),
            pending_ejects: Mutex::new(BTreeMap::new()),
            log,
        }
    }

    pub fn attach(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;

        pio.register(PCI_HOTPLUG_IO_ADDR, PCI_HOTPLUG_IO_LEN.into(), piofn)
            .unwrap();
    }

    pub fn hotplug(&self, bdf: Bdf) -> Result<(), Error> {
        let bus = bdf.bus.into();
        let dev: u8 = bdf.location.dev.into();

        self.pci_up
            .get(&bus)
            .ok_or(Error::InvalidBdf(bdf))?
            .fetch_or(1 << dev, Ordering::AcqRel);

        probes::pci_hotplug!(|| (bus, dev));

        Ok(())
    }

    pub fn hot_unplug(
        &self,
        bdf: Bdf,
        ejfn: Option<EjectFn>,
    ) -> Result<(), Error> {
        let bus = bdf.bus.into();
        let dev: u8 = bdf.location.dev.into();

        self.pci_down
            .get(&bus)
            .ok_or(Error::InvalidBdf(bdf))?
            .fetch_or(1 << dev, Ordering::AcqRel);

        if let Some(ejfn) = ejfn {
            self.pending_ejects.lock().unwrap().insert(bdf, ejfn);
        }

        probes::pci_hotplug_unplug!(|| (bus, dev));

        Ok(())
    }

    fn pio_rw(&self, _: u16, mut rwo: RWOp) {
        REGS.process(&mut rwo, |reg, rwo| match rwo {
            RWOp::Read(ro) => self.pio_read(reg, ro),
            RWOp::Write(wo) => self.pio_write(reg, wo),
        });
    }

    fn pio_read(&self, reg: &Register, ro: &mut ReadOp) {
        let Ok(bus) = u8::try_from(self.bus.load(Ordering::Acquire)) else {
            return;
        };
        match reg {
            Register::PciUp => {
                if let Some(pci_up) = self.pci_up.get(&bus) {
                    let bits = pci_up.load(Ordering::Acquire);
                    ro.write_u32(bits);
                    probes::pci_hotplug_io_read!(|| (reg.name(), bits));
                }
            }
            Register::PciDown => {
                if let Some(pci_down) = self.pci_down.get(&bus) {
                    let bits = pci_down.load(Ordering::Acquire);
                    ro.write_u32(bits);
                    probes::pci_hotplug_io_read!(|| (reg.name(), bits));
                }
            }
            Register::Eject => {
                // B0EJ is never read by the guest.
                ro.fill(0);
                probes::pci_hotplug_io_read!(|| (reg.name(), 0));
            }
            Register::BusNumber => {
                let bits = self.bus.load(Ordering::Acquire);
                ro.write_u32(bits);
                probes::pci_hotplug_io_read!(|| (reg.name(), bits));
            }
        };
    }

    fn pio_write(&self, reg: &Register, wo: &mut WriteOp) {
        let Ok(bus) = u8::try_from(self.bus.load(Ordering::Acquire)) else {
            return;
        };
        match reg {
            Register::PciUp | Register::PciDown => {
                // Guests should not write to PCIU or PCID.
                let bits = wo.read_u32();
                probes::pci_hotplug_io_write!(|| (reg.name(), bits));
            }
            Register::Eject => {
                if let Some(eject) = self.eject.get(&bus) {
                    let bits = wo.read_u32();
                    eject.store(bits, Ordering::Release);

                    probes::pci_hotplug_io_write!(|| (reg.name(), bits));

                    self.eject(bus, bits);
                }
            }
            Register::BusNumber => {
                let bits = wo.read_u32();
                self.bus.store(bits, Ordering::Release);
                probes::pci_hotplug_io_write!(|| (reg.name(), bits,));
            }
        };
    }

    fn eject(&self, bus: u8, ej_bits: u32) {
        let Ok(dev) = u8::try_from(ej_bits.trailing_zeros()) else {
            slog::warn!(
                self.log,
                "Invalid PCI device ejection bitmask {ej_bits}"
            );
            return;
        };

        let Some(bdf) = Bdf::new(bus, dev, 0) else {
            slog::warn!(self.log, "Invalid BDF {bus}.{dev}.0 for PCI ejection");
            return;
        };

        if let Some(ejfn) = self.pending_ejects.lock().unwrap().remove(&bdf) {
            ejfn();
        } else {
            slog::info!(self.log, "Ignoring PCI device ejection for BDF {bdf}");
        }

        probes::pci_hotplug_eject!(|| (bus, dev));
    }
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn pci_hotplug(bus: u8, dev: u8) {}
    fn pci_hotplug_unplug(bus: u8, dev: u8) {}
    fn pci_hotplug_eject(bus: u8, dev: u8) {}
    fn pci_hotplug_io_read(reg: &str, value: u32) {}
    fn pci_hotplug_io_write(reg: &str, value: u32) {}
}
