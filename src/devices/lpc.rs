use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::devices::uart::{LpcUart, UartSock, REGISTER_LEN};
use crate::intr_pins::IsaPIC;
use crate::pci;
use crate::pio::{PioBus, PioDev};

const PIR_OFFSET: usize = 0x60;
const PIR_LEN: usize = 8;
const PIR_END: usize = PIR_OFFSET + PIR_LEN;

const PIR_MASK_DISABLE: u8 = 0x80;
const PIR_MASK_IRQ: u8 = 0x0f;

const COM1_PORT: u16 = 0x3f8;
const COM2_PORT: u16 = 0x2f8;
const COM1_IRQ: u8 = 4;
const COM2_IRQ: u8 = 3;

pub struct Piix3Bhyve {
    reg_pir: Mutex<[u8; 8]>,
    pic: Weak<IsaPIC>,
    uart_com1: Arc<LpcUart>,
    // uart_com2: Arc<LpcUart>,
}
impl Piix3Bhyve {
    pub fn new(
        pic: &Arc<IsaPIC>,
        pio_bus: &PioBus,
        com1_sock: Arc<UartSock>,
    ) -> Arc<pci::DeviceInst<Self>> {
        let com1 = LpcUart::new(com1_sock, pic.pin_handle(COM1_IRQ).unwrap());
        // let com2 = LpcUart::new(pic.pin_handle(COM2_IRQ).unwrap());

        // UART interrupts mapped the same on ioapic and atpic
        pic.set_irq_atpic(COM1_IRQ, Some(COM1_IRQ));
        // pic.set_irq_atpic(COM2_IRQ, Some(COM2_IRQ));
        pio_bus
            .register(
                COM1_PORT,
                REGISTER_LEN as u16,
                Arc::downgrade(&com1) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();
        // pio_bus.register(
        //     COM2_PORT,
        //     REGISTER_LEN as u16,
        //     Arc::downgrade(&com2) as Weak<dyn PioDev>,
        //     0,
        // );

        let this = Self {
            reg_pir: Mutex::new([0u8; 8]),
            pic: Arc::downgrade(pic),
            uart_com1: com1,
            // uart_com2: com2,
        };
        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7000,
            class: 0x06,
            subclass: 0x01,
            ..Default::default()
        })
        .add_custom_cfg(PIR_OFFSET as u8, PIR_LEN as u8)
        .finish(this)
    }

    fn write_pir(&self, pic: &IsaPIC, idx: usize, val: u8) {
        let mut regs = self.reg_pir.lock().unwrap();
        if regs[idx] != val {
            let disabled = (val & PIR_MASK_DISABLE) == 0;
            let irq = val & PIR_MASK_IRQ;

            // XXX better integrate with PCI interrupt routing
            if !disabled && Self::valid_irq(irq) {
                pic.set_irq_atpic(16 + idx as u8, Some(irq));
            } else {
                pic.set_irq_atpic(16 + idx as u8, None);
            }
            regs[idx] = val;
        }
    }
    fn valid_irq(irq: u8) -> bool {
        // Existing ACPI tables allow 3-7, 9-12, 14-15
        matches!(irq, 3..=7 | 9..=12 | 14 | 15)
    }

    pub fn set_pir_defaults(&self) {
        let pic = Weak::upgrade(&self.pic).unwrap();
        let default_irqs: &[u8] = &[5, 6, 9, 10, 11, 12, 14, 15];
        for (idx, irq) in default_irqs.iter().enumerate() {
            self.write_pir(&pic, idx, *irq);
        }
    }
}
impl pci::Device for Piix3Bhyve {
    fn cfg_read(&self, ro: &mut ReadOp) {
        assert!(ro.offset >= PIR_OFFSET && ro.offset + ro.buf.len() < PIR_END);
        let off = ro.offset - PIR_OFFSET;
        let reg = self.reg_pir.lock().unwrap();
        ro.buf.copy_from_slice(&reg[off..(off + ro.buf.len())]);
    }
    fn cfg_write(&self, wo: &WriteOp) {
        assert!(wo.offset >= PIR_OFFSET && wo.offset + wo.buf.len() < PIR_END);
        if let Some(pic) = Weak::upgrade(&self.pic) {
            let off = wo.offset - PIR_OFFSET;
            for (i, val) in wo.buf.iter().enumerate() {
                self.write_pir(&pic, i + off, *val);
            }
        }
    }
}
