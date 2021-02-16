use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::{BarPlacer, Chipset};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::pci::{self, INTxPinID, PioCfgDecoder, BDF};
use crate::hw::ps2ctrl::PS2Ctrl;
use crate::hw::uart::{self, LpcUart};
use crate::intr_pins::{IntrPin, LegacyPIC, LegacyPin};
use crate::pio::{PioBus, PioDev};
use crate::util::regmap::RegMap;
use crate::util::self_arc::*;
use crate::vmm::VmmHdl;

use lazy_static::lazy_static;

const LEGACY_PIC_PINS: u8 = 32;

pub struct I440Fx {
    pic: Arc<LegacyPIC>,
    pci_bus: Mutex<pci::Bus>,
    pci_cfg: PioCfgDecoder,

    lnk_pins: [Arc<LNKPin>; 4],
    sci_pin: Arc<LNKPin>,

    sa_cell: SelfArcCell<Self>,
}
impl I440Fx {
    pub fn create(
        hdl: Arc<VmmHdl>,
        pio: &PioBus,
        cfg_lpc: impl FnOnce(&Piix3Lpc),
    ) -> Arc<Self> {
        let pic = LegacyPIC::new(Arc::clone(&hdl));

        let sci_pin = Arc::new(LNKPin::new());
        sci_pin.reassign(pic.pin_handle(SCI_IRQ));

        let mut this = Arc::new(Self {
            pic,
            pci_bus: Mutex::new(pci::Bus::new()),
            pci_cfg: PioCfgDecoder::new(),

            lnk_pins: [
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
            ],
            sci_pin,

            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);

        let hbdev = Piix4HostBridge::create();
        let lpcdev = Piix3Lpc::create(Arc::downgrade(&this), &this.pic, pio);
        let pmdev = Piix3PM::create(hdl.as_ref(), pio);

        lpcdev.with_inner(cfg_lpc);

        this.pci_attach(BDF::new(0, 0, 0), hbdev);
        this.pci_attach(BDF::new(0, 1, 0), lpcdev);
        this.pci_attach(BDF::new(0, 1, 3), pmdev);

        this
    }

    fn set_lnk_route(&self, idx: usize, irq: Option<u8>) {
        assert!(idx <= 3);
        self.lnk_pins[idx].reassign(irq.and_then(|i| self.pic.pin_handle(i)));
    }

    fn route_lintr(&self, bdf: &BDF) -> (INTxPinID, Arc<dyn IntrPin>) {
        let intx_pin = match (bdf.func() + 1) % 4 {
            1 => INTxPinID::INTA,
            2 => INTxPinID::INTB,
            3 => INTxPinID::INTC,
            4 => INTxPinID::INTD,
            _ => panic!(),
        };
        // D->A->B->C starting at 0:0.0
        let pin_route = (bdf.dev() + intx_pin as u8 + 2) % 4;
        (
            intx_pin,
            Arc::clone(&self.lnk_pins[pin_route as usize]) as Arc<dyn IntrPin>,
        )
    }
    fn place_bars(&self) {
        let bus = self.pci_bus.lock().unwrap();

        let mut bar_placer = BarPlacer::new();
        bar_placer.add_avail_pio(0xc000, 0x4000);
        bar_placer.add_avail_mmio(0xe0000000, 0x10000000);

        for (slot, func, dev) in bus.iter() {
            dev.bar_for_each(&mut |bar, def| {
                bar_placer.add_bar((slot, func, bar), def);
            });
        }
        let remain = bar_placer.place(|(slot, func, bar), addr| {
            println!(
                "placing {:?} @ {:x} for 0:{:x}:{:x}",
                bar, addr, slot, func
            );
            let dev = bus.device_at(slot, func).unwrap();
            dev.bar_place(bar, addr as u64);
        });
        if let Some((pio, mmio)) = remain {
            panic!("Unfulfilled BAR allocations! pio:{} mmio:{}", pio, mmio);
        }
    }
}
impl Chipset for I440Fx {
    fn pci_attach(&self, bdf: BDF, dev: Arc<dyn pci::Endpoint>) {
        assert!(bdf.bus() == 0);

        dev.attach(&|| self.route_lintr(&bdf));
        let mut bus = self.pci_bus.lock().unwrap();
        bus.attach(bdf.dev(), bdf.func(), dev);
    }
    fn pci_finalize(&self, ctx: &DispCtx) {
        let cfg_pio = self.self_weak() as Weak<dyn PioDev>;
        ctx.mctx.with_pio(|pio| {
            let cfg_pio2 = Weak::clone(&cfg_pio);
            pio.register(pci::PORT_PCI_CONFIG_ADDR, 4, cfg_pio, 0).unwrap();
            pio.register(pci::PORT_PCI_CONFIG_DATA, 4, cfg_pio2, 0).unwrap();
        });
        self.place_bars();
    }
}
impl PioDev for I440Fx {
    fn pio_rw(&self, port: u16, _ident: usize, rwo: RWOp, ctx: &DispCtx) {
        match port {
            pci::PORT_PCI_CONFIG_ADDR => {
                self.pci_cfg.service_addr(rwo);
            }
            pci::PORT_PCI_CONFIG_DATA => {
                self.pci_cfg.service_data(rwo, |bdf, rwo| {
                    if bdf.bus() != 0 {
                        return None;
                    }
                    let bus = self.pci_bus.lock().unwrap();
                    if let Some(dev) = bus.device_at(bdf.dev(), bdf.func()) {
                        let dev = Arc::clone(dev);
                        drop(bus);
                        dev.cfg_rw(rwo, ctx);
                        // let opname = match rwo {
                        //     RWOp::Read(_) => "cfgread",
                        //     RWOp::Write(_) => "cfgwrite",
                        // };
                        // println!(
                        //     "{} bus:{} device:{} func:{} off:{:x}, data:{:x?}",
                        //     opname,
                        //     bdf.bus(),
                        //     bdf.dev(),
                        //     bdf.func(),
                        //     rwo.offset(),
                        //     rwo.buf()
                        // );
                        Some(())
                    } else {
                        None
                    }
                });
            }
            _ => {
                panic!();
            }
        }
    }
}
impl SelfArc for I440Fx {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

struct LNKPin {
    inner: Mutex<LNKPinInner>,
}
struct LNKPinInner {
    asserted: bool,
    pin: Option<LegacyPin>,
}
impl LNKPin {
    fn new() -> Self {
        Self { inner: Mutex::new(LNKPinInner { asserted: false, pin: None }) }
    }
    fn reassign(&self, new_pin: Option<LegacyPin>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(old_pin) = inner.pin.as_ref() {
            if inner.asserted {
                old_pin.deassert()
            }
        }

        if let Some(pin) = new_pin.as_ref() {
            if inner.asserted {
                pin.assert()
            }
        }
        inner.pin = new_pin;
    }
}
impl IntrPin for LNKPin {
    fn assert(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.asserted = true;
        if let Some(pin) = inner.pin.as_ref() {
            pin.assert();
        }
    }
    fn deassert(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.asserted = false;
        if let Some(pin) = inner.pin.as_ref() {
            pin.deassert();
        }
    }
    fn pulse(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(pin) = inner.pin.as_ref() {
            pin.pulse();
        }
    }
    fn is_asserted(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.asserted
    }
}

const PIR_OFFSET: usize = 0x60;
const PIR_LEN: usize = 4;
const PIR_END: usize = PIR_OFFSET + PIR_LEN;

const PIR_MASK_DISABLE: u8 = 0x80;
const PIR_MASK_IRQ: u8 = 0x0f;

const SCI_IRQ: u8 = 0x9;

fn valid_pir_irq(irq: u8) -> bool {
    // Existing ACPI tables allow 3-7, 9-12, 14-15
    matches!(irq, 3..=7 | 9..=12 | 14 | 15)
}

struct Piix4HostBridge {}
impl Piix4HostBridge {
    pub fn create() -> Arc<pci::DeviceInst> {
        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x1237,
            class: 0x06,
            ..Default::default()
        })
        .finish(Arc::new(Self {}))
    }
}
impl pci::Device for Piix4HostBridge {}

const COM1_PORT: u16 = 0x3f8;
const COM2_PORT: u16 = 0x2f8;
const COM3_PORT: u16 = 0x3e8;
const COM4_PORT: u16 = 0x2e8;
const COM1_IRQ: u8 = 4;
const COM2_IRQ: u8 = 3;
const COM3_IRQ: u8 = 4;
const COM4_IRQ: u8 = 3;

const PORT_FAST_A20: u16 = 0x92;
const LEN_FAST_A20: u16 = 1;
const PORT_POST_CODE: u16 = 0x80;
const LEN_POST_CODE: u16 = 1;

pub struct Piix3Lpc {
    reg_pir: Mutex<[u8; PIR_LEN]>,
    post_code: AtomicU8,
    uart_com1: Arc<LpcUart>,
    uart_com2: Arc<LpcUart>,
    uart_com3: Arc<LpcUart>,
    uart_com4: Arc<LpcUart>,
    ps2_ctrl: Arc<PS2Ctrl>,
    chipset: Weak<I440Fx>,
}
impl Piix3Lpc {
    pub fn create(
        chipset: Weak<I440Fx>,
        pic: &LegacyPIC,
        pio_bus: &PioBus,
    ) -> Arc<pci::DeviceInst> {
        let com1 = LpcUart::new(pic.pin_handle(COM1_IRQ).unwrap());
        let com2 = LpcUart::new(pic.pin_handle(COM2_IRQ).unwrap());
        let com3 = LpcUart::new(pic.pin_handle(COM3_IRQ).unwrap());
        let com4 = LpcUart::new(pic.pin_handle(COM4_IRQ).unwrap());

        pio_bus
            .register(
                COM1_PORT,
                uart::REGISTER_LEN as u16,
                Arc::downgrade(&com1) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();
        pio_bus
            .register(
                COM2_PORT,
                uart::REGISTER_LEN as u16,
                Arc::downgrade(&com2) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();
        pio_bus
            .register(
                COM3_PORT,
                uart::REGISTER_LEN as u16,
                Arc::downgrade(&com2) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();
        pio_bus
            .register(
                COM4_PORT,
                uart::REGISTER_LEN as u16,
                Arc::downgrade(&com2) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();

        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio_bus, pic);

        let this = Arc::new(Self {
            reg_pir: Mutex::new([0u8; PIR_LEN]),
            post_code: AtomicU8::new(0),
            uart_com1: com1,
            uart_com2: com2,
            uart_com3: com3,
            uart_com4: com4,
            ps2_ctrl,
            chipset,
        });

        pio_bus
            .register(
                PORT_FAST_A20,
                LEN_FAST_A20,
                Arc::downgrade(&this) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();
        pio_bus
            .register(
                PORT_POST_CODE,
                LEN_POST_CODE,
                Arc::downgrade(&this) as Weak<dyn PioDev>,
                0,
            )
            .unwrap();

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

    pub fn config_uarts<F>(&self, f: F)
    where
        F: FnOnce(&Arc<LpcUart>, &Arc<LpcUart>, &Arc<LpcUart>, &Arc<LpcUart>),
    {
        f(&self.uart_com1, &self.uart_com2, &self.uart_com3, &self.uart_com4);
    }

    fn write_pir(&self, idx: usize, val: u8) {
        assert!(idx < PIR_LEN);

        let mut regs = self.reg_pir.lock().unwrap();
        if regs[idx] != val {
            let disabled = (val & PIR_MASK_DISABLE) != 0;
            let irq = val & PIR_MASK_IRQ;

            // XXX better integrate with PCI interrupt routing
            let chipset = Weak::upgrade(&self.chipset).unwrap();
            if !disabled && valid_pir_irq(irq) {
                chipset.set_lnk_route(idx, Some(irq));
            } else {
                chipset.set_lnk_route(idx, None);
            }
            regs[idx] = val;
        }
    }
}
impl pci::Device for Piix3Lpc {
    fn cfg_rw(&self, region: u8, rwo: RWOp) {
        assert_eq!(region as usize, PIR_OFFSET);
        assert!(rwo.offset() + rwo.len() <= PIR_END - PIR_OFFSET);

        match rwo {
            RWOp::Read(ro) => {
                let off = ro.offset();
                let reg = self.reg_pir.lock().unwrap();
                ro.write_bytes(&reg[off..(off + ro.len())]);
            }
            RWOp::Write(wo) => {
                let off = wo.offset();
                for i in 0..wo.len() {
                    self.write_pir(off + i, wo.read_u8());
                }
            }
        }
    }
}
impl PioDev for Piix3Lpc {
    fn pio_rw(&self, port: u16, _ident: usize, rwo: RWOp, _ctx: &DispCtx) {
        match port {
            PORT_FAST_A20 => {
                match rwo {
                    RWOp::Read(ro) => {
                        // A20 is always enabled
                        ro.write_u8(0x02);
                    }
                    RWOp::Write(wo) => {
                        let _ = wo.read_u8();
                        // TODO: handle FAST_INIT request
                    }
                }
            }
            PORT_POST_CODE => match rwo {
                RWOp::Read(ro) => {
                    ro.write_u8(self.post_code.load(Ordering::SeqCst));
                }
                RWOp::Write(wo) => {
                    self.post_code.store(wo.read_u8(), Ordering::SeqCst);
                }
            },
            _ => {}
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

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PmReg {
    PmSts,
    PmEn,
    PmCntrl,
    PmTmr,
    GpSts,
    GpEn,
    PCntrl,
    PLvl2,
    PLvl3,
    GlbSts,
    DevSts,
    GlbEn,
    GlbCtl,
    DevCtl,
    GpiReg,
    GpoReg,
    Reserved,
}

lazy_static! {
    static ref PM_REGS: RegMap<PmReg> = {
        let layout = [
            (PmReg::PmSts, 2),
            (PmReg::PmEn, 2),
            (PmReg::PmCntrl, 2),
            (PmReg::Reserved, 2),
            (PmReg::PmTmr, 4),
            (PmReg::GpSts, 2),
            (PmReg::GpEn, 2),
            (PmReg::PCntrl, 4),
            (PmReg::PLvl2, 1),
            (PmReg::PLvl3, 1),
            (PmReg::Reserved, 2),
            (PmReg::GlbSts, 2),
            (PmReg::Reserved, 2),
            (PmReg::DevSts, 4),
            (PmReg::GlbEn, 2),
            (PmReg::Reserved, 6),
            (PmReg::GlbCtl, 4),
            (PmReg::DevCtl, 4),
            (PmReg::GpiReg, 4),
            (PmReg::GpoReg, 4),
            (PmReg::Reserved, 8),
        ];
        RegMap::create_packed(
            PMBASE_LEN as usize,
            &layout,
            Some(PmReg::Reserved),
        )
    };
}
bitflags! {
    #[derive(Default)]
    struct PmSts: u16 {
        const PWRBTN_STS = 1 << 8;
    }
}
bitflags! {
    #[derive(Default)]
    struct PmEn: u16 {
        const PWRBTN_EN = 1 << 8;
    }
}
bitflags! {
    #[derive(Default)]
    struct PmCntrl: u16 {
        const SCI_EN = 1;
        const SUS_TYP = 0b111 << 10;
        const SUS_EN = 1 << 13;

    }
}

struct PMRegs {
    pm_base: u16,
    pm_status: PmSts,
    pm_ena: PmEn,
    pm_ctrl: PmCntrl,
}
impl Default for PMRegs {
    fn default() -> Self {
        Self {
            pm_base: PMBASE_DEFAULT,
            pm_status: PmSts::empty(),
            pm_ena: PmEn::empty(),
            pm_ctrl: PmCntrl::empty(),
        }
    }
}

pub struct Piix3PM {
    regs: Mutex<PMRegs>,
    sa_cell: SelfArcCell<Self>,
}
impl Piix3PM {
    pub fn create(hdl: &VmmHdl, pio: &PioBus) -> Arc<pci::DeviceInst> {
        let regs = PMRegs::default();
        let mut this = Arc::new(Self {
            regs: Mutex::new(regs),
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);

        // XXX: static registration for now
        pio.register(
            PMBASE_DEFAULT,
            PMBASE_LEN,
            Arc::downgrade(&this) as Weak<dyn PioDev>,
            0,
        )
        .unwrap();
        hdl.pmtmr_locate(PMBASE_DEFAULT + 0x8).unwrap();

        pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7113,
            class: 0x06,
            subclass: 0x80,
            ..Default::default()
        })
        .add_custom_cfg(PMCFG_OFFSET as u8, PMCFG_LEN as u8)
        .finish(this)
    }
    fn pmcfg_read(&self, id: &PmCfg, ro: &mut ReadOp) {
        match id {
            PmCfg::PmRegMisc => {
                // Report IO space as enabled
                ro.write_u8(0x1);
            }
            PmCfg::PmBase => {
                let regs = self.regs.lock().unwrap();

                // LSB hardwired to 1 to indicate PMBase in IO space
                ro.write_u32(regs.pm_base as u32 | 0x1);
            }
            _ => {
                // XXX: report everything else as zeroed
                ro.fill(0);
            }
        }
    }
    fn pmcfg_write(&self, id: &PmCfg, _wo: &WriteOp) {
        // XXX: ignore writes for now
        println!("ignored PM cfg write to {:?}", id);
    }
    fn pmreg_read(&self, id: &PmReg, ro: &mut ReadOp) {
        let regs = self.regs.lock().unwrap();
        match id {
            PmReg::PmSts => {
                ro.write_u16(regs.pm_status.bits());
            }
            PmReg::PmEn => {
                ro.write_u16(regs.pm_ena.bits());
            }
            PmReg::PmCntrl => {
                ro.write_u16(regs.pm_ctrl.bits());
            }

            PmReg::PmTmr
            | PmReg::GpSts
            | PmReg::GpEn
            | PmReg::PCntrl
            | PmReg::PLvl2
            | PmReg::PLvl3
            | PmReg::GlbSts
            | PmReg::DevSts
            | PmReg::GlbEn
            | PmReg::GlbCtl
            | PmReg::DevCtl
            | PmReg::GpiReg
            | PmReg::GpoReg => {
                // TODO: flesh out the rest of PM emulation
                println!("unhandled PM read {:x}", ro.offset());
                ro.fill(0);
            }
            PmReg::Reserved => {
                ro.fill(0);
            }
        }
    }
    fn pmreg_write(&self, id: &PmReg, wo: &mut WriteOp) {
        let mut regs = self.regs.lock().unwrap();
        match id {
            PmReg::PmSts => {
                let val = PmSts::from_bits_truncate(wo.read_u16());
                // status bits are W1C
                regs.pm_status.remove(val);
            }
            PmReg::PmEn => {
                regs.pm_ena = PmEn::from_bits_truncate(wo.read_u16());
            }
            PmReg::PmCntrl => {
                regs.pm_ctrl = PmCntrl::from_bits_truncate(wo.read_u16());
                if regs.pm_ctrl.contains(PmCntrl::SUS_EN) {
                    // SUS_EN is write-only and should always read 0
                    regs.pm_ctrl.remove(PmCntrl::SUS_EN);

                    let suspend_type = (regs.pm_ctrl & PmCntrl::SUS_TYP).bits();
                    if suspend_type == 0 {
                        // 0b000 corresponds to soft-off
                        // XXX: initiate power-off
                        eprintln!("poweroff");
                    }
                }
            }
            PmReg::PmTmr
            | PmReg::GpSts
            | PmReg::GpEn
            | PmReg::PCntrl
            | PmReg::PLvl2
            | PmReg::PLvl3
            | PmReg::GlbSts
            | PmReg::DevSts
            | PmReg::GlbEn
            | PmReg::GlbCtl
            | PmReg::DevCtl
            | PmReg::GpiReg
            | PmReg::GpoReg => {
                println!("unhandled PM write {:x}", wo.offset());
            }
            PmReg::Reserved => {}
        }
    }
}
impl pci::Device for Piix3PM {
    fn cfg_rw(&self, region: u8, mut rwo: RWOp) {
        assert_eq!(region as usize, PMCFG_OFFSET);

        PM_CFG_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmcfg_read(id, ro),
            RWOp::Write(wo) => self.pmcfg_write(id, wo),
        })
    }
}
impl PioDev for Piix3PM {
    fn pio_rw(&self, _port: u16, _ident: usize, mut rwo: RWOp, _ctx: &DispCtx) {
        PM_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmreg_read(id, ro),
            RWOp::Write(wo) => self.pmreg_write(id, wo),
        });
    }
}
impl SelfArc for Piix3PM {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}
