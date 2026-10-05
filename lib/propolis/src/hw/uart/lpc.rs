// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use super::uart16550::{migrate, Uart};
use crate::chardev::*;
use crate::common::*;
use crate::common::{DeviceMetadata, DeviceMetadataMap};
use crate::firmware::acpi;
use crate::intr_pins::IntrPin;
use crate::migrate::*;
use crate::pio::{PioBus, PioFn};

use acpi_tables::{aml, Aml, AmlSink};

// Low Pin Count UART

pub const REGISTER_LEN: usize = 8;

struct UartState {
    uart: Uart,
    irq_pin: Box<dyn IntrPin>,
    auto_discard: bool,

    // In the absence of better interfaces for chardev save/restore behavior,
    // allow the device to be coarsely paused (dropping all reads and writes).
    paused: bool,
}

impl UartState {
    fn sync_intr_pin(&self) {
        if self.uart.intr_state() {
            self.irq_pin.assert()
        } else {
            self.irq_pin.deassert()
        }
    }
}

pub struct LpcUart {
    state: Mutex<UartState>,
    notify_readable: NotifierCell<dyn Source>,
    notify_writable: NotifierCell<dyn Sink>,
}

impl LpcUart {
    pub fn new(irq_pin: Box<dyn IntrPin>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(UartState {
                uart: Uart::new(),
                irq_pin,
                auto_discard: true,
                paused: false,
            }),
            notify_readable: NotifierCell::new(),
            notify_writable: NotifierCell::new(),
        })
    }
    pub fn attach(self: &Arc<Self>, bus: &PioBus, port: u16) {
        let this = self.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| this.pio_rw(rwo))
            as Arc<PioFn>;
        bus.register(port, REGISTER_LEN as u16, piofn).unwrap();
    }
    fn pio_rw(&self, rwo: RWOp) {
        assert!(rwo.offset() < REGISTER_LEN);
        assert!(rwo.len() != 0);
        let mut state = self.state.lock().unwrap();
        let readable_before = state.uart.is_readable();
        let writable_before = state.uart.is_writable();

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(state.uart.reg_read(ro.offset() as u8));
            }
            RWOp::Write(wo) => {
                state.uart.reg_write(wo.offset() as u8, wo.read_u8());
            }
        }
        if state.auto_discard {
            while let Some(_val) = state.uart.data_read() {}
        }

        state.sync_intr_pin();

        let read_notify = !readable_before && state.uart.is_readable();
        let write_notify = !writable_before && state.uart.is_writable();

        // The uart state lock cannot be held while dispatching notifications
        // since those callbacks could immediately attempt to read/write the
        // pending data.
        drop(state);
        if read_notify {
            self.notify_readable.notify(self as &dyn Source);
        }
        if write_notify {
            self.notify_writable.notify(self as &dyn Sink);
        }
    }
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.uart.reset();
        state.sync_intr_pin();
    }
}

impl Sink for LpcUart {
    fn write(&self, data: u8) -> bool {
        let mut state = self.state.lock().unwrap();

        if state.paused {
            return false;
        }

        let res = state.uart.data_write(data);
        state.sync_intr_pin();
        res
    }
    fn set_notifier(&self, f: Option<SinkNotifier>) {
        self.notify_writable.set(f);
    }
}
impl Source for LpcUart {
    fn read(&self) -> Option<u8> {
        let mut state = self.state.lock().unwrap();

        if state.paused {
            return None;
        }

        let res = state.uart.data_read();
        state.sync_intr_pin();
        res
    }
    fn discard(&self, count: usize) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut discarded = 0;
        while discarded < count {
            if let Some(_val) = state.uart.data_read() {
                discarded += 1;
            } else {
                break;
            }
        }
        state.sync_intr_pin();
        discarded
    }
    fn set_notifier(&self, f: Option<SourceNotifier>) {
        self.notify_readable.set(f);
    }
    fn set_autodiscard(&self, active: bool) {
        let mut state = self.state.lock().unwrap();
        state.auto_discard = active;
    }
}

impl Lifecycle for LpcUart {
    fn type_name(&self) -> &'static str {
        "lpc-uart"
    }
    fn reset(&self) {
        LpcUart::reset(self);
    }
    fn migrate(&self) -> Migrator<'_> {
        Migrator::Single(self)
    }

    fn pause(&self) {
        let mut state = self.state.lock().unwrap();
        state.paused = true;
    }

    fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        state.paused = false;
    }

    fn as_dsdt_generator(&self) -> Option<&dyn acpi::DsdtGenerator> {
        Some(self)
    }
}
impl MigrateSingle for LpcUart {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let state = self.state.lock().unwrap();
        Ok(state.uart.export().into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data = offer.parse::<migrate::Uart16550V1>()?;
        let mut state = self.state.lock().unwrap();
        state.uart.import(&data)?;
        state.irq_pin.import_state(state.uart.intr_state());
        Ok(())
    }
}

impl acpi::DsdtGenerator for LpcUart {
    fn dsdt_scope(&self) -> acpi::DsdtScope {
        acpi::DsdtScope::Lpc
    }

    fn device_type(&self) -> Option<acpi::DsdtDeviceType> {
        Some(acpi::DsdtDeviceType::Uart)
    }

    // This AML code is inherited from the original EDK2 static tables.
    fn to_aml_bytes(
        &self,
        acpi_variant: acpi::AcpiVariant,
        device_metadata: &DeviceMetadataMap,
        sink: &mut dyn AmlSink,
    ) {
        let metadata = device_metadata.get(self).unwrap();

        if acpi_variant == acpi::AcpiVariant::V0 {
            // The original EDK2-based tables only defined COM1 and COM2, even
            // though VMs usually have 4 serial ports.
            if metadata.num > 2 {
                return;
            }
        }

        aml::Device::new(
            aml::Path::new(&format!("UAR{}", metadata.num)),
            vec![
                &acpi::aml::names::hid(&aml::EISAName::new(
                    acpi::aml::devids::COM_PORT_16550A,
                )),
                &acpi::aml::names::ddn(&format!("COM{}", metadata.num)),
                &acpi::aml::names::uid(&metadata.num),
                &acpi::aml::names::crs(&aml::ResourceTemplate::new(vec![
                    &acpi::aml::io_port(metadata.port, 1, REGISTER_LEN as u8),
                    &aml::Irq::new(true, false, false, metadata.irq),
                ])),
            ],
        )
        .to_aml_bytes(sink);
    }
}

impl DeviceMetadata for LpcUart {
    type Metadata = LpcUartMetadata;
}

pub struct LpcUartMetadata {
    num: u8,
    port: u16,
    irq: u8,
}
impl LpcUartMetadata {
    pub fn new(num: u8, port: u16, irq: u8) -> Self {
        Self { num, port, irq }
    }
}
