// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this

pub mod bits;

use crate::common::RWOp;
use crate::intr_pins::IntrPin;
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Eq, PartialEq, Error)]
pub enum Error {
    #[error("invalid offset")]
    InvalidOffset,
}

bitflags! {
    #[derive(Default, Copy, Clone, Debug)]
    struct GpeRegister: u8 {}
}

bitflags! {
    #[derive(Default, Copy, Clone, Debug)]
    struct PciStatus: u8 {}
}

struct GpeRegisterBlock {
    sts: Mutex<GpeRegister>,
    en: Mutex<GpeRegister>,
}
impl GpeRegisterBlock {
    fn new() -> Self {
        let en = GpeRegister::empty().into();
        let sts = GpeRegister::empty().into();
        Self { en, sts }
    }
}

enum GpeRegisters {
    Gpe0Sts = 0,
    Gpe0En = 2,

    Gpe1Sts = 1,
    Gpe1En = 3,
}
impl TryFrom<usize> for GpeRegisters {
    type Error = ();

    fn try_from(v: usize) -> Result<Self, Self::Error> {
        match v {
            x if x == GpeRegisters::Gpe0Sts as usize => {
                Ok(GpeRegisters::Gpe0Sts)
            }
            x if x == GpeRegisters::Gpe0En as usize => Ok(GpeRegisters::Gpe0En),
            x if x == GpeRegisters::Gpe1Sts as usize => {
                Ok(GpeRegisters::Gpe1Sts)
            }
            x if x == GpeRegisters::Gpe1En as usize => Ok(GpeRegisters::Gpe1En),
            _ => Err(()),
        }
    }
}

pub struct ACPI {
    gpe: [GpeRegisterBlock; 2],

    pci_up: Mutex<PciStatus>,
    pci_down: Mutex<PciStatus>,

    sci_pin: Arc<dyn IntrPin>,
}
impl ACPI {
    pub fn new(sci_pin: Arc<dyn IntrPin>) -> Self {
        let gpe = [GpeRegisterBlock::new(), GpeRegisterBlock::new()];
        let pci_up = PciStatus::empty().into();
        let pci_down = PciStatus::empty().into();

        // Always enable GPE0_EN[0x2].
        gpe[0].en.lock().unwrap().insert(GpeRegister::from_bits_retain(0x2));

        Self { gpe, pci_up, pci_down, sci_pin }
    }

    pub fn plug_device(&self) {
        // Set GPE0_STS[0x2] and pulse SCI to run ACPI method \_GPE._E01.
        let mut gpe0_sts = self.gpe[0].sts.lock().unwrap();
        gpe0_sts.insert(GpeRegister::from_bits_retain(0x2));
        self.sci_pin.pulse();
    }

    pub fn pio_rw(&self, port: u16, rwo: RWOp) {
        match port {
            bits::GPE_ADDR => self.gpe_rw(rwo),
            bits::PCI_HOTPLUG_ADDR => self.pci_hotplug_rw(rwo),
            _ => panic!(),
        }
    }

    fn gpe_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                let reg = self
                    .gpe_reg_from_offset(ro.offset())
                    .unwrap()
                    .lock()
                    .unwrap();

                ro.write_u8(reg.bits());
            }
            RWOp::Write(wo) => {
                let bits = GpeRegister::from_bits_retain(wo.read_u8());
                let mut reg = self
                    .gpe_reg_from_offset(wo.offset())
                    .unwrap()
                    .lock()
                    .unwrap();

                match wo.offset().try_into() {
                    Ok(GpeRegisters::Gpe0Sts) | Ok(GpeRegisters::Gpe1Sts) => {
                        reg.remove(bits);
                    }
                    Ok(GpeRegisters::Gpe0En) | Ok(GpeRegisters::Gpe1En) => {
                        reg.insert(bits);
                    }
                    Err(e) => panic!("{:?}", e),
                };
            }
        }
    }

    fn pci_hotplug_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                println!("{:?}", ro);
                let pci_up = self.pci_up.lock().unwrap();
                ro.write_u8(pci_up.bits());
            }
            RWOp::Write(wo) => {
                println!("{:?}", wo);
                let mut pci = match wo.offset() {
                    0 => self.pci_up.lock().unwrap(),
                    4 => self.pci_down.lock().unwrap(),
                    _ => panic!(),
                };
                pci.insert(PciStatus::from_bits_retain(wo.read_u8()));
                println!("{:?}", pci);
            }
        }
    }

    fn gpe_reg_from_offset(
        &self,
        offset: usize,
    ) -> Result<&Mutex<GpeRegister>, Error> {
        match offset.try_into() {
            Ok(GpeRegisters::Gpe0Sts) => Ok(&self.gpe[0].sts),
            Ok(GpeRegisters::Gpe0En) => Ok(&self.gpe[0].en),
            Ok(GpeRegisters::Gpe1Sts) => Ok(&self.gpe[1].sts),
            Ok(GpeRegisters::Gpe1En) => Ok(&self.gpe[1].en),
            Err(_) => Err(Error::InvalidOffset),
        }
    }
}
