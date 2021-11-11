use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use super::Chipset;
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::hw::bhyve::BhyvePmTimer;
use crate::hw::ibmpc;
use crate::hw::pci::{self, Bdf, BusNum, INTxPinID, PioCfgDecoder};
use crate::instance;
use crate::intr_pins::{IntrPin, LegacyPIC, LegacyPin};
use crate::inventory;
use crate::migrate::Migrate;
use crate::pio::{PioBus, PioFn};
use crate::util::regmap::RegMap;
use crate::vmm::{Machine, VmmHdl};

use erased_serde::Serialize;
use lazy_static::lazy_static;

const HB_DEV: u8 = 0;
const HB_FUNC: u8 = 0;
const LPC_DEV: u8 = 1;
const LPC_FUNC: u8 = 0;
const PM_DEV: u8 = 1;
const PM_FUNC: u8 = 3;

pub struct I440Fx {
    pci_bus: pci::Bus,
    pci_cfg: PioCfgDecoder,
    irq_config: Arc<IrqConfig>,

    dev_hb: Arc<Piix4HostBridge>,
    dev_lpc: Arc<Piix3Lpc>,
    dev_pm: Arc<Piix3PM>,

    pm_timer: Arc<BhyvePmTimer>,
}
impl I440Fx {
    pub fn create(machine: &Machine) -> Arc<Self> {
        let hdl = machine.hdl.clone();
        let irq_config = IrqConfig::create(hdl);

        let this = Arc::new(Self {
            pci_bus: pci::Bus::new(
                BusNum::new(0).unwrap(),
                &machine.bus_pio,
                &machine.bus_mmio,
            ),
            pci_cfg: PioCfgDecoder::new(),
            irq_config: irq_config.clone(),

            dev_hb: Piix4HostBridge::create(),
            dev_lpc: Piix3Lpc::create(irq_config),
            dev_pm: Piix3PM::create(),

            pm_timer: BhyvePmTimer::create(),
        });

        this.pci_attach(
            Bdf::new(0, HB_DEV, HB_FUNC).unwrap(),
            this.dev_hb.clone(),
        );
        this.pci_attach(
            Bdf::new(0, LPC_DEV, LPC_FUNC).unwrap(),
            this.dev_lpc.clone(),
        );
        this.pci_attach(
            Bdf::new(0, PM_DEV, PM_FUNC).unwrap(),
            this.dev_pm.clone(),
        );

        // Attach chipset devices
        let pio = &machine.bus_pio;
        let hdl = &machine.hdl;
        this.dev_lpc.attach(pio);
        this.dev_pm.attach(pio, hdl);

        let pio_dev = Arc::clone(&this);
        let piofn = Arc::new(move |port: u16, rwo: RWOp, ctx: &DispCtx| {
            pio_dev.pio_rw(port, rwo, ctx)
        }) as Arc<PioFn>;
        pio.register(
            pci::bits::PORT_PCI_CONFIG_ADDR,
            pci::bits::LEN_PCI_CONFIG_ADDR,
            Arc::clone(&piofn),
        )
        .unwrap();
        pio.register(
            pci::bits::PORT_PCI_CONFIG_DATA,
            pci::bits::LEN_PCI_CONFIG_DATA,
            piofn,
        )
        .unwrap();

        this
    }

    fn route_lintr(&self, bdf: &Bdf) -> (INTxPinID, Arc<dyn IntrPin>) {
        let intx_pin = match (bdf.func.get() + 1) % 4 {
            0 => INTxPinID::IntA,
            1 => INTxPinID::IntB,
            2 => INTxPinID::IntC,
            3 => INTxPinID::IntD,
            _ => panic!(),
        };
        // D->A->B->C starting at 0:0.0
        let pin_route = (bdf.dev.get() + intx_pin as u8 + 2) % 4;
        (intx_pin, self.irq_config.intr_pin(pin_route as usize))
    }

    fn pio_rw(&self, port: u16, rwo: RWOp, ctx: &DispCtx) {
        match port {
            pci::bits::PORT_PCI_CONFIG_ADDR => {
                self.pci_cfg.service_addr(rwo);
            }
            pci::bits::PORT_PCI_CONFIG_DATA => {
                self.pci_cfg.service_data(rwo, |bdf, rwo| {
                    if bdf.bus.get() != 0 {
                        return None;
                    }
                    if let Some(dev) = self.pci_bus.device_at(*bdf) {
                        // This is pretty noisy during boot
                        // let opname = match rwo {
                        //     RWOp::Read(_) => "cfgread",
                        //     RWOp::Write(_) => "cfgwrite",
                        // };
                        // slog::trace!(ctx.log, "PCI {}", opname;
                        //     "bdf" => %bdf, "offset" => rwo.offset());

                        dev.cfg_rw(rwo, ctx);
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
impl Chipset for I440Fx {
    fn pci_attach(&self, bdf: Bdf, dev: Arc<dyn pci::Endpoint>) {
        assert!(bdf.bus.get() == 0);

        self.pci_bus.attach(bdf, dev, Some(self.route_lintr(&bdf)));
    }
    fn irq_pin(&self, irq: u8) -> Option<LegacyPin> {
        self.irq_config.pic.pin_handle(irq)
    }
}
impl Migrate for I440Fx {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::I440TopV1 { pci_cfg_addr: self.pci_cfg.addr() })
    }
}
impl Entity for I440Fx {
    fn type_name(&self) -> &'static str {
        "chipset-i440fx"
    }
    fn child_register(&self) -> Option<Vec<inventory::ChildRegister>> {
        Some(vec![
            inventory::ChildRegister::new(&self.dev_hb, None),
            inventory::ChildRegister::new(&self.dev_lpc, None),
            inventory::ChildRegister::new(&self.dev_pm, None),
            inventory::ChildRegister::new(&self.pm_timer, None),
        ])
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
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

struct IrqConfig {
    pic: Arc<LegacyPIC>,

    lnk_pins: [Arc<LNKPin>; 4],

    #[allow(unused)]
    // XXX: wire up SCI notifications
    sci_pin: Arc<LNKPin>,
}
impl IrqConfig {
    fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        let pic = LegacyPIC::new(hdl);
        let sci_pin = Arc::new(LNKPin::new());
        sci_pin.reassign(pic.pin_handle(SCI_IRQ));
        Arc::new(Self {
            pic,
            lnk_pins: [
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
                Arc::new(LNKPin::new()),
            ],
            sci_pin,
        })
    }
    fn set_lnk_route(&self, idx: usize, irq: Option<u8>) {
        assert!(idx <= 3);
        self.lnk_pins[idx].reassign(irq.and_then(|i| self.pic.pin_handle(i)));
    }
    fn intr_pin(&self, idx: usize) -> Arc<dyn IntrPin> {
        assert!(idx <= 3);
        Arc::clone(&self.lnk_pins[idx]) as Arc<dyn IntrPin>
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

struct Piix4HostBridge {
    pci_state: pci::DeviceState,
}
impl Piix4HostBridge {
    pub fn create() -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x1237,
            class: 0x06,
            ..Default::default()
        })
        .finish();
        Arc::new(Self { pci_state })
    }
}
impl pci::Device for Piix4HostBridge {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}
impl Entity for Piix4HostBridge {
    fn type_name(&self) -> &'static str {
        "pci-piix4-hb"
    }
    fn reset(&self, _ctx: &DispCtx) {
        self.pci_state.reset(self);
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for Piix4HostBridge {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::Piix4HostBridgeV1 {
            pci_state: self.pci_state.export(),
        })
    }
}

pub struct Piix3Lpc {
    pci_state: pci::DeviceState,
    reg_pir: Mutex<[u8; PIR_LEN]>,
    post_code: AtomicU8,
    irq_config: Arc<IrqConfig>,
}
impl Piix3Lpc {
    fn create(irq_config: Arc<IrqConfig>) -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7000,
            class: 0x06,
            subclass: 0x01,
            ..Default::default()
        })
        .add_custom_cfg(PIR_OFFSET as u8, PIR_LEN as u8)
        .finish();

        Arc::new(Self {
            pci_state,
            reg_pir: Mutex::new([0u8; PIR_LEN]),
            post_code: AtomicU8::new(0),
            irq_config,
        })
    }

    fn attach(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp, ctx: &DispCtx| {
            this.pio_rw(port, rwo, ctx)
        }) as Arc<PioFn>;
        pio.register(
            ibmpc::PORT_FAST_A20,
            ibmpc::LEN_FAST_A20,
            Arc::clone(&piofn),
        )
        .unwrap();
        pio.register(ibmpc::PORT_POST_CODE, ibmpc::LEN_POST_CODE, piofn)
            .unwrap();
    }

    fn pio_rw(&self, port: u16, rwo: RWOp, _ctx: &DispCtx) {
        match port {
            ibmpc::PORT_FAST_A20 => {
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
            ibmpc::PORT_POST_CODE => match rwo {
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

    fn write_pir(&self, idx: usize, val: u8) {
        assert!(idx < PIR_LEN);

        let mut regs = self.reg_pir.lock().unwrap();
        if regs[idx] != val {
            let disabled = (val & PIR_MASK_DISABLE) != 0;
            let irq = val & PIR_MASK_IRQ;

            // XXX better integrate with PCI interrupt routing
            if !disabled && valid_pir_irq(irq) {
                self.irq_config.set_lnk_route(idx, Some(irq));
            } else {
                self.irq_config.set_lnk_route(idx, None);
            }
            regs[idx] = val;
        }
    }
}
impl pci::Device for Piix3Lpc {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn cfg_rw(&self, region: u8, rwo: RWOp, _ctx: &DispCtx) {
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
impl Entity for Piix3Lpc {
    fn type_name(&self) -> &'static str {
        "pci-piix3-lpc"
    }
    fn reset(&self, _ctx: &DispCtx) {
        self.pci_state.reset(self);
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for Piix3Lpc {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        let pir = self.reg_pir.lock().unwrap();
        Box::new(migrate::Piix3LpcV1 {
            pci_state: self.pci_state.export(),
            pir_regs: *pir,
            post_code: self.post_code.load(Ordering::Acquire),
        })
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

// Offset within PMBASE region corresponding to PmTmr register
const PM_TMR_OFFSET: u16 = 0x8;

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
impl PMRegs {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

pub struct Piix3PM {
    pci_state: pci::DeviceState,
    regs: Mutex<PMRegs>,
}
impl Piix3PM {
    pub fn create() -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: 0x8086,
            device_id: 0x7113,
            class: 0x06,
            subclass: 0x80,
            ..Default::default()
        })
        .add_custom_cfg(PMCFG_OFFSET as u8, PMCFG_LEN as u8)
        .finish();

        Arc::new(Self { pci_state, regs: Mutex::new(PMRegs::default()) })
    }

    fn attach(self: &Arc<Self>, pio: &PioBus, hdl: &VmmHdl) {
        // XXX: static registration for now
        let this = Arc::clone(&self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp, ctx: &DispCtx| {
            this.pio_rw(port, rwo, ctx)
        }) as Arc<PioFn>;
        pio.register(PMBASE_DEFAULT, PMBASE_LEN, piofn).unwrap();
        hdl.pmtmr_locate(PMBASE_DEFAULT + PM_TMR_OFFSET).unwrap();
    }

    fn pio_rw(&self, _port: u16, mut rwo: RWOp, ctx: &DispCtx) {
        PM_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmreg_read(id, ro, ctx),
            RWOp::Write(wo) => self.pmreg_write(id, wo, ctx),
        });
    }

    fn pmcfg_read(&self, id: &PmCfg, ro: &mut ReadOp, ctx: &DispCtx) {
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
                slog::info!(ctx.log, "piix3pm ignored cfg read";
                    "offset" => ro.offset(), "register" => ?id);
                ro.fill(0);
            }
        }
    }
    fn pmcfg_write(&self, id: &PmCfg, _wo: &WriteOp, ctx: &DispCtx) {
        // XXX: ignore writes for now
        slog::info!(ctx.log, "piix3pm ignored cfg write";
            "offset" => _wo.offset(), "register" => ?id);
    }
    fn pmreg_read(&self, id: &PmReg, ro: &mut ReadOp, ctx: &DispCtx) {
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
                slog::info!(ctx.log, "piix3pm unhandled read";
                    "offset" => ro.offset(), "register" => ?id);
                ro.fill(0);
            }
            PmReg::Reserved => {
                ro.fill(0);
            }
        }
    }
    fn pmreg_write(&self, id: &PmReg, wo: &mut WriteOp, ctx: &DispCtx) {
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
                        ctx.trigger_suspend(
                            instance::SuspendKind::Halt,
                            instance::SuspendSource::Device("ACPI PmCntrl"),
                        );
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
                slog::info!(ctx.log, "piix3pm unhandled write";
                    "offset" => wo.offset(), "register" => ?id);
            }
            PmReg::Reserved => {}
        }
    }
    fn reset(&self, ctx: &DispCtx) {
        let mut regs = self.regs.lock().unwrap();
        regs.reset();
        // Make sure PM timer is attached to the right IO port
        // TODO: error handling?
        ctx.mctx.hdl().pmtmr_locate(PMBASE_DEFAULT + PM_TMR_OFFSET).unwrap();
    }
}
impl pci::Device for Piix3PM {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
    fn cfg_rw(&self, region: u8, mut rwo: RWOp, ctx: &DispCtx) {
        assert_eq!(region as usize, PMCFG_OFFSET);

        PM_CFG_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmcfg_read(id, ro, ctx),
            RWOp::Write(wo) => self.pmcfg_write(id, wo, ctx),
        })
    }
}
impl Entity for Piix3PM {
    fn type_name(&self) -> &'static str {
        "pci-piix3-pm"
    }
    fn reset(&self, ctx: &DispCtx) {
        self.pci_state.reset(self);
        self.reset(ctx);
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for Piix3PM {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        let regs = self.regs.lock().unwrap();
        Box::new(migrate::Piix3PmV1 {
            pci_state: self.pci_state.export(),
            pm_base: regs.pm_base,
            pm_status: regs.pm_status.bits(),
            pm_ena: regs.pm_ena.bits(),
            pm_ctrl: regs.pm_ctrl.bits(),
        })
    }
}

mod migrate {
    use crate::hw::pci::migrate::PciStateV1;
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct I440TopV1 {
        pub pci_cfg_addr: u32,
    }
    #[derive(Serialize)]
    pub struct Piix4HostBridgeV1 {
        pub pci_state: PciStateV1,
    }
    #[derive(Serialize)]
    pub struct Piix3LpcV1 {
        pub pci_state: PciStateV1,
        pub pir_regs: [u8; super::PIR_LEN],
        pub post_code: u8,
    }
    #[derive(Serialize)]
    pub struct Piix3PmV1 {
        pub pci_state: PciStateV1,
        pub pm_base: u16,
        pub pm_status: u16,
        pub pm_ena: u16,
        pub pm_ctrl: u16,
    }
}
