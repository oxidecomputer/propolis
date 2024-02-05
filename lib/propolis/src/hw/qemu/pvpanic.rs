// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::pio::{PioBus, PioFn};

/// Implements the QEMU [pvpanic device], which
/// may be used by guests to notify the host when a kernel panic has occurred.
///
/// QEMU exposes the pvpanic virtual device as a device on the ISA bus (I/O port
/// 0x505), a PCI device, and through ACPI. Currently, Propolis only implements
/// the ISA bus pvpanic device, but the PCI device may be implemented in the
/// future.
///
/// [pvpanic device]: https://www.qemu.org/docs/master/specs/pvpanic.html
#[derive(Debug)]
pub struct QemuPvpanic {
    counts: Mutex<PanicCounts>,
    log: slog::Logger,
}

/// Counts the number of guest kernel panics reported using the [`QemuPvpanic`]
/// virtual device.
#[derive(Copy, Clone, Debug)]
pub struct PanicCounts {
    /// Counts the number of guest kernel panics handled by the host.
    pub host_handled: usize,
    /// Counts the number of guest kernel panics handled by the guest.
    pub guest_handled: usize,
}

pub const DEVICE_NAME: &str = "qemu-pvpanic";

/// Indicates that a guest panic has happened and should be processed by the
/// host
const HOST_HANDLED: u8 = 0b01;
/// Indicates a guest panic has happened and will be handled by the guest; the
/// host should record it or report it, but should not affect the execution of
/// the guest.
const GUEST_HANDLED: u8 = 0b10;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn pvpanic_pio_write(value: u8) {}
}

impl QemuPvpanic {
    const IOPORT: u16 = 0x505;

    pub fn create(log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            counts: Mutex::new(PanicCounts {
                host_handled: 0,
                guest_handled: 0,
            }),
            log,
        })
    }

    /// Attaches this pvpanic device to the provided [`PioBus`].
    pub fn attach_pio(self: &Arc<Self>, pio: &PioBus) {
        let piodev = self.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| piodev.pio_rw(rwo))
            as Arc<PioFn>;
        pio.register(Self::IOPORT, 1, piofn).unwrap();
    }

    /// Returns the current panic counts reported by the guest.
    pub fn panic_counts(&self) -> PanicCounts {
        *self.counts.lock().unwrap()
    }

    fn pio_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(HOST_HANDLED | GUEST_HANDLED);
            }
            RWOp::Write(wo) => {
                let value = wo.read_u8();
                probes::pvpanic_pio_write!(|| value);
                let host_handled = value & HOST_HANDLED != 0;
                let guest_handled = value & GUEST_HANDLED != 0;
                slog::debug!(
                    self.log,
                    "guest kernel panic";
                    "host_handled" => host_handled,
                    "guest_handled" => guest_handled,
                );

                let mut counts = self.counts.lock().unwrap();

                if host_handled {
                    counts.host_handled += 1;
                }

                if guest_handled {
                    counts.guest_handled += 1;
                }
            }
        }
    }
}

impl Lifecycle for QemuPvpanic {
    fn type_name(&self) -> &'static str {
        DEVICE_NAME
    }
}
