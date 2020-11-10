use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::devices::ps2ctrl::PS2Ctrl;
use crate::devices::uart::{LpcUart, UartSock, REGISTER_LEN};
use crate::dispatch::DispCtx;
use crate::intr_pins::IsaPIC;
use crate::pci;
use crate::pio::{PioBus, PioDev};
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;
use crate::vmm::MachineCtx;

use byteorder::{ByteOrder, LE};
use lazy_static::lazy_static;

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
    ps2_ctrl: Arc<PS2Ctrl>,
}
impl Piix3Bhyve {
    pub fn new(
        pic: &Arc<IsaPIC>,
        pio_bus: &PioBus,
        com1_sock: Arc<UartSock>,
    ) -> Arc<pci::DeviceInst> {
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

        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio_bus, pic);

        let this = Self {
            reg_pir: Mutex::new([0u8; 8]),
            pic: Arc::downgrade(pic),
            uart_com1: com1,
            // uart_com2: com2,
            ps2_ctrl,
        };

        // Make configurable later
        this.set_pir_defaults();

        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7000,
            class: 0x06,
            subclass: 0x01,
            ..Default::default()
        })
        .add_custom_cfg(PIR_OFFSET as u8, PIR_LEN as u8)
        .finish_plain(this)
    }

    fn write_pir(&self, pic: &IsaPIC, idx: usize, val: u8) {
        let mut regs = self.reg_pir.lock().unwrap();
        if regs[idx] != val {
            let disabled = (val & PIR_MASK_DISABLE) != 0;
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
    fn cfg_rw(&self, region: u8, rwo: &mut RWOp) {
        assert_eq!(region as usize, PIR_OFFSET);
        assert!(rwo.offset() + rwo.len() <= PIR_END - PIR_OFFSET);

        match rwo {
            RWOp::Read(ro) => {
                let off = ro.offset;
                let reg = self.reg_pir.lock().unwrap();
                ro.buf.copy_from_slice(&reg[off..(off + ro.buf.len())]);
            }
            RWOp::Write(wo) => {
                if let Some(pic) = Weak::upgrade(&self.pic) {
                    let off = wo.offset;
                    for (i, val) in wo.buf.iter().enumerate() {
                        self.write_pir(&pic, i + off, *val);
                    }
                }
            }
        }
    }
}

const PMCFG_OFFSET: usize = 0x40;
const PMCFG_LEN: usize = 0x98;

const PMBASE_DEFAULT: u16 = 0xb000;
const PMBASE_LEN: u16 = 0x40;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PmCfg {
    PmBase,
    CountA,
    CountB,
    GpInputCtl,
    DevResD,
    DevActA,
    DevActB,
    DevResA,
    DevResB,
    DevResC,
    DevResE,
    DevResF,
    DevResG,
    DevResH,
    DevResI,
    DevResJ,
    PmRegMisc,
    SmbusBase,
    SmbusHostCfg,
    SmbusSlaveCmd,
    SmbusSlaveShadow1,
    SmbusSlaveShadow2,
    SmbusRev,
    Reserved,
}
lazy_static! {
    static ref PM_CFG_REGS: RegMap<PmCfg> = {
        let layout = [
            (PmCfg::PmBase, 4),
            (PmCfg::CountA, 4),
            (PmCfg::CountB, 4),
            (PmCfg::GpInputCtl, 4),
            (PmCfg::DevResD, 2),
            (PmCfg::Reserved, 2),
            (PmCfg::DevActA, 4),
            (PmCfg::DevActB, 4),
            (PmCfg::DevResA, 4),
            (PmCfg::DevResB, 4),
            (PmCfg::DevResC, 4),
            (PmCfg::DevResE, 4),
            (PmCfg::DevResF, 4),
            (PmCfg::DevResG, 2),
            (PmCfg::Reserved, 2),
            (PmCfg::DevResH, 4),
            (PmCfg::DevResI, 4),
            (PmCfg::DevResJ, 4),
            (PmCfg::PmRegMisc, 1),
            (PmCfg::Reserved, 15),
            (PmCfg::SmbusBase, 4),
            (PmCfg::Reserved, 62),
            (PmCfg::SmbusHostCfg, 1),
            (PmCfg::SmbusSlaveCmd, 1),
            (PmCfg::SmbusSlaveShadow1, 1),
            (PmCfg::SmbusSlaveShadow2, 1),
            (PmCfg::SmbusRev, 1),
            (PmCfg::Reserved, 1),
        ];
        RegMap::create_packed(PMCFG_LEN, &layout, Some(PmCfg::Reserved))
    };
}

struct PMRegs {
    pm_base: u16,
}
impl Default for PMRegs {
    fn default() -> Self {
        Self { pm_base: PMBASE_DEFAULT }
    }
}

pub struct Piix3PM {
    regs: Mutex<PMRegs>,
    sa_cell: SelfArcCell<Self>,
}
impl Piix3PM {
    pub fn create(mctx: &MachineCtx) -> Arc<pci::DeviceInst> {
        let regs = PMRegs::default();
        let mut this = Arc::new(Self {
            regs: Mutex::new(regs),
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);

        // XXX: static registration for now
        mctx.with_pio(|pio_bus| {
            pio_bus
                .register(
                    PMBASE_DEFAULT,
                    PMBASE_LEN,
                    Arc::downgrade(&this) as Weak<dyn PioDev>,
                    0,
                )
                .unwrap();
        });
        mctx.with_hdl(|hdl| hdl.pmtmr_locate(PMBASE_DEFAULT + 0x8).unwrap());

        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7113,
            class: 0x06,
            subclass: 0x80,
            ..Default::default()
        })
        .add_custom_cfg(PMCFG_OFFSET as u8, PMCFG_LEN as u8)
        .finish_arc(this)
    }
    fn pmcfg_read(&self, id: &PmCfg, ro: &mut ReadOp) {
        match id {
            PmCfg::PmRegMisc => {
                // Report IO space as enabled
                ro.buf[0] = 0x1;
            }
            PmCfg::PmBase => {
                let regs = self.regs.lock().unwrap();

                // LSB hardwired to 1 to indicate PMBase in IO space
                LE::write_u32(ro.buf, regs.pm_base as u32 | 0x1);
            }
            _ => {
                // XXX: report everything else as zeroed
                for b in ro.buf.iter_mut() {
                    *b = 0;
                }
            }
        }
    }
    fn pmcfg_write(&self, id: &PmCfg, _wo: &WriteOp) {
        // XXX: ignore writes for now
        println!("ignored PM cfg write to {:?}", id);
    }
}
impl pci::Device for Piix3PM {
    fn cfg_rw(&self, region: u8, rwo: &mut RWOp) {
        assert_eq!(region as usize, PMCFG_OFFSET);

        PM_CFG_REGS.process(rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmcfg_read(id, ro),
            RWOp::Write(wo) => self.pmcfg_write(id, wo),
        })
    }
}
impl PioDev for Piix3PM {
    fn pio_in(&self, port: u16, ident: usize, ro: &mut ReadOp, ctx: &DispCtx) {
        println!("unhandled PM read {:x}", ro.offset);
    }
    fn pio_out(&self, port: u16, ident: usize, wo: &WriteOp, ctx: &DispCtx) {
        println!("unhandled PM write {:x}", wo.offset);
    }
}
impl SelfArc for Piix3PM {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}
