// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::hw::bhyve::{
    BhyveAtPic, BhyveAtPit, BhyveIoApic, BhyvePmTimer, BhyveRtc,
};
use crate::hw::chipset::Chipset;
use crate::hw::ibmpc;
use crate::hw::ids::pci::{
    PIIX3_ISA_DEV_ID, PIIX3_ISA_SUB_DEV_ID, PIIX4_HB_DEV_ID,
    PIIX4_HB_SUB_DEV_ID, PIIX4_PM_DEV_ID, PIIX4_PM_SUB_DEV_ID, VENDOR_INTEL,
    VENDOR_OXIDE,
};
use crate::hw::pci::topology::{LogicalBusId, RoutedBusId};
use crate::hw::pci::{
    self, Bdf, INTxPinID, LintrCfg, PcieCfgDecoder, PioCfgDecoder,
};
use crate::intr_pins::{IntrPin, LegacyPIC, LegacyPin, NoOpPin};
use crate::migrate::*;
use crate::mmio::MmioFn;
use crate::pio::{PioBus, PioFn};
use crate::util::regmap::RegMap;
use crate::vmm::{Machine, VmmHdl};

use lazy_static::lazy_static;

const ADDR_PCIE_ECAM_REGION: usize = 0xe000_0000;
const LEN_PCI_ECAM_REGION: usize = 0x1000_0000;

pub const DEFAULT_HB_BDF: Bdf = Bdf::new_unchecked(0, 0, 0);
pub const DEFAULT_LPC_BDF: Bdf = Bdf::new_unchecked(0, 1, 0);
pub const DEFAULT_PM_BDF: Bdf = Bdf::new_unchecked(0, 1, 3);

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

#[derive(Default)]
pub struct Opts {
    pub enable_pcie: bool,
    pub power_pin: Option<Arc<dyn IntrPin>>,
    pub reset_pin: Option<Arc<dyn IntrPin>>,
}

pub struct I440FxHostBridge {
    pci_state: pci::DeviceState,

    pci_topology: Arc<pci::topology::Topology>,
    pci_cfg: PioCfgDecoder,
    pcie_cfg: Option<PcieCfgDecoder>,

    pin_power: Arc<dyn IntrPin>,
    pin_reset: Arc<dyn IntrPin>,
}
impl I440FxHostBridge {
    pub fn create(
        pci_topology: Arc<pci::topology::Topology>,
        opts: Opts,
    ) -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_INTEL,
            device_id: PIIX4_HB_DEV_ID,
            sub_vendor_id: VENDOR_OXIDE,
            sub_device_id: PIIX4_HB_SUB_DEV_ID,
            class: pci::bits::CLASS_BRIDGE,
            subclass: pci::bits::SUBCLASS_BRIDGE_HOST,
            ..Default::default()
        })
        .finish();

        let pin_power = opts.power_pin.unwrap_or_else(|| Arc::new(NoOpPin {}));
        let pin_reset = opts.reset_pin.unwrap_or_else(|| Arc::new(NoOpPin {}));

        let pci_cfg = PioCfgDecoder::new();
        let pcie_cfg = opts.enable_pcie.then(|| {
            PcieCfgDecoder::new(pci::bits::PCIE_MAX_BUSES_PER_ECAM_REGION)
        });

        Arc::new(Self {
            pci_state,

            pci_topology,
            pci_cfg,
            pcie_cfg,

            pin_power,
            pin_reset,
        })
    }

    pub fn attach(self: &Arc<Self>, machine: &Machine) {
        let pio = &machine.bus_pio;
        let pio_dev = Arc::clone(self);
        let piofn =
            Arc::new(move |port: u16, rwo: RWOp| pio_dev.pio_rw(port, rwo))
                as Arc<PioFn>;
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

        if self.pcie_cfg.is_some() {
            let mmio = &machine.bus_mmio;
            let mmio_dev = Arc::clone(self);
            let mmio_ecam_fn = Arc::new(move |_addr: usize, rwo: RWOp| {
                mmio_dev.pcie_ecam_rw(rwo);
            }) as Arc<MmioFn>;
            mmio.register(
                ADDR_PCIE_ECAM_REGION,
                LEN_PCI_ECAM_REGION,
                mmio_ecam_fn,
            )
            .unwrap();
        }
    }

    fn pci_cfg_rw(&self, bdf: &Bdf, rwo: RWOp) -> Option<()> {
        self.pci_topology.pci_cfg_rw(
            RoutedBusId(bdf.bus.get()),
            bdf.location,
            rwo,
        )
    }

    fn pio_rw(&self, port: u16, rwo: RWOp) {
        match port {
            pci::bits::PORT_PCI_CONFIG_ADDR => {
                self.pci_cfg.service_addr(rwo);
            }
            pci::bits::PORT_PCI_CONFIG_DATA => self
                .pci_cfg
                .service_data(rwo, |bdf, rwo| self.pci_cfg_rw(bdf, rwo)),
            _ => {
                panic!();
            }
        }
    }

    fn pcie_ecam_rw(&self, rwo: RWOp) {
        let pcie_cfg = self
            .pcie_cfg
            .as_ref()
            .expect("PCIe cfg decoder present when ECAM is enabled");

        pcie_cfg.service(rwo, |bdf, rwo| self.pci_cfg_rw(bdf, rwo));
    }
}
impl pci::Device for I440FxHostBridge {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}
impl Lifecycle for I440FxHostBridge {
    fn type_name(&self) -> &'static str {
        "pci-i440fx-hb"
    }
    fn reset(&self) {
        self.pci_state.reset(self);
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl Chipset for I440FxHostBridge {
    fn pci_attach(
        &self,
        bdf: Bdf,
        dev: Arc<dyn pci::Endpoint>,
        lintr_cfg: Option<LintrCfg>,
    ) {
        self.pci_topology
            .pci_attach(
                LogicalBusId(bdf.bus.get()),
                bdf.location,
                dev,
                lintr_cfg,
            )
            .unwrap();
    }
    fn power_pin(&self) -> Arc<dyn IntrPin> {
        self.pin_power.clone()
    }
    fn reset_pin(&self) -> Arc<dyn IntrPin> {
        self.pin_reset.clone()
    }
}
impl MigrateMulti for I440FxHostBridge {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        MigrateMulti::export(&self.pci_state, output, ctx)?;
        output.push(
            migrate::I440FxHostBridgeV1 { pci_cfg_addr: self.pci_cfg.addr() }
                .into(),
        )
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        MigrateMulti::import(&self.pci_state, offer, ctx)?;
        let data: migrate::I440FxHostBridgeV1 = offer.take()?;
        self.pci_cfg.set_addr(data.pci_cfg_addr);
        Ok(())
    }
}

pub struct Piix3Lpc {
    pci_state: pci::DeviceState,

    pub pic: Arc<BhyveAtPic>,
    pub pit: Arc<BhyveAtPit>,
    pub ioapic: Arc<BhyveIoApic>,
    pub rtc: Arc<BhyveRtc>,

    reg_pir: Mutex<[u8; PIR_LEN]>,
    irq_config: Arc<IrqConfig>,
    post_code: AtomicU8,
}
impl Piix3Lpc {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_INTEL,
            device_id: PIIX3_ISA_DEV_ID,
            sub_vendor_id: VENDOR_OXIDE,
            sub_device_id: PIIX3_ISA_SUB_DEV_ID,
            class: pci::bits::CLASS_BRIDGE,
            subclass: pci::bits::SUBCLASS_BRIDGE_ISA,
            ..Default::default()
        })
        .add_custom_cfg(PIR_OFFSET as u8, PIR_LEN as u8)
        .finish();

        let irq_config = IrqConfig::create(hdl.clone());

        Arc::new(Self {
            pci_state,

            pic: BhyveAtPic::create(hdl.clone()),
            pit: BhyveAtPit::create(hdl.clone()),
            ioapic: BhyveIoApic::create(hdl.clone()),
            rtc: BhyveRtc::create(hdl.clone()),

            reg_pir: Mutex::new([0u8; PIR_LEN]),
            post_code: AtomicU8::new(0),
            irq_config,
        })
    }

    pub fn attach(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;
        pio.register(
            ibmpc::PORT_FAST_A20,
            ibmpc::LEN_FAST_A20,
            Arc::clone(&piofn),
        )
        .unwrap();
        pio.register(ibmpc::PORT_POST_CODE, ibmpc::LEN_POST_CODE, piofn)
            .unwrap();
    }

    fn pio_rw(&self, port: u16, rwo: RWOp) {
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

    pub fn route_lintr(&self, bdf: Bdf) -> Option<LintrCfg> {
        if bdf.bus.get() != 0 {
            return None;
        }
        let intx_pin = match (bdf.location.func.get() + 1) % 4 {
            0 => INTxPinID::IntA,
            1 => INTxPinID::IntB,
            2 => INTxPinID::IntC,
            3 => INTxPinID::IntD,
            _ => unreachable!(),
        };
        // D->A->B->C starting at 0:0.0
        let pin_route = (bdf.location.dev.get() + intx_pin as u8 + 2) % 4;
        Some((intx_pin, self.irq_config.intr_pin(pin_route as usize)))
    }

    pub fn irq_pin(&self, irq: u8) -> Option<Box<dyn IntrPin>> {
        self.irq_config
            .pic
            .pin_handle(irq)
            .map(|pin| Box::new(pin) as Box<dyn IntrPin>)
    }
}
impl pci::Device for Piix3Lpc {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

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
impl Lifecycle for Piix3Lpc {
    fn type_name(&self) -> &'static str {
        "pci-piix3-lpc"
    }
    fn reset(&self) {
        self.pci_state.reset(self);
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for Piix3Lpc {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let pir = self.reg_pir.lock().unwrap();
        output.push(
            migrate::Piix3LpcV1 {
                pir_regs: *pir,
                post_code: self.post_code.load(Ordering::Acquire),
            }
            .into(),
        )?;
        drop(pir);

        MigrateMulti::export(&self.pci_state, output, ctx)?;

        MigrateMulti::export(self.pic.as_ref(), output, ctx)?;
        MigrateMulti::export(self.pit.as_ref(), output, ctx)?;
        MigrateMulti::export(self.ioapic.as_ref(), output, ctx)?;
        MigrateMulti::export(self.rtc.as_ref(), output, ctx)?;
        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let input: migrate::Piix3LpcV1 = offer.take()?;

        // The device is paused during import. Acquiring the PIR lock will
        // add an implicit barrier, so relaxed ordering is OK here.
        self.post_code.store(input.post_code, Ordering::Relaxed);
        *self.reg_pir.lock().unwrap() = input.pir_regs;

        MigrateMulti::import(&self.pci_state, offer, ctx)?;

        MigrateMulti::import(self.pic.as_ref(), offer, ctx)?;
        MigrateMulti::import(self.pit.as_ref(), offer, ctx)?;
        MigrateMulti::import(self.ioapic.as_ref(), offer, ctx)?;
        MigrateMulti::import(self.rtc.as_ref(), offer, ctx)?;

        Ok(())
    }
}

const PMCFG_OFFSET: usize = 0x40;
const PMCFG_LEN: usize = 0x98;

const PMBASE_DEFAULT: u16 = 0xb000;
const PMBASE_LEN: u16 = 0x40;
const SMBBASE_DEFAULT: u16 = 0xb100;
// const SMBBASE_LEN: u16 = 0x40;

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
    #[derive(Default, Copy, Clone)]
    struct PmSts: u16 {
        const PWRBTN_STS = 1 << 8;
    }
}
bitflags! {
    #[derive(Default, Copy, Clone)]
    struct PmEn: u16 {
        const PWRBTN_EN = 1 << 8;
    }
}
bitflags! {
    #[derive(Default, Copy, Clone)]
    struct PmCntrl: u16 {
        const SCI_EN = 1;
        const SUS_TYP = 0b111 << 10;
        const SUS_EN = 1 << 13;

    }
}

bitflags! {
    #[derive(Default)]
    struct DevResA: u32 {
        // Enable bus decodes for keyboard controller
        const KBC_EN_DEV11 = 1 << 28;
    }
}
bitflags! {
    #[derive(Default)]
    struct DevResB: u32 {
        // PCI access to keyboard controller
        const KBC_EIO_EN = 1 << 30;
        // PCI access to FDC
        const EIO_EN_DEV5 = 1 << 29;
    }
}
bitflags! {
    #[derive(Default)]
    struct DevResC: u32 {
        // PCI access to ttyb
        const EIO_EN_DEV7 = 1 << 31;
        // Configure ttyb for COM2 port
        const COMB_DEC_SEL_COM2 = 0b001 << 28;
        // PCI access to ttya
        const EIO_EN_DEV6 = 1 << 27;
        // Configure ttya for COM1 port
        const COMA_DEC_SEL_COM1 = 0b000 << 24;
    }
}

// Offset within PMBASE region corresponding to PmTmr register
const PM_TMR_OFFSET: u16 = 0x8;

#[derive(Clone, Copy)]
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
    fn pmtimer_port(&self) -> u16 {
        self.pm_base.checked_add(PM_TMR_OFFSET).unwrap()
    }
}

impl From<PMRegs> for migrate::Piix3PmV1 {
    fn from(value: PMRegs) -> Self {
        Self {
            pm_base: value.pm_base,
            pm_status: value.pm_status.bits(),
            pm_ena: value.pm_ena.bits(),
            pm_ctrl: value.pm_ctrl.bits(),
        }
    }
}
impl TryFrom<migrate::Piix3PmV1> for PMRegs {
    type Error = MigrateStateError;

    fn try_from(value: migrate::Piix3PmV1) -> Result<Self, Self::Error> {
        let mut regs = Self::default();

        regs.pm_base = value.pm_base;
        regs.pm_status =
            PmSts::from_bits(value.pm_status).ok_or_else(|| {
                MigrateStateError::ImportFailed(format!(
                    "PIIX3 pm_status: failed to import saved value {:#x}",
                    value.pm_status,
                ))
            })?;
        regs.pm_ena = PmEn::from_bits(value.pm_ena).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "PIIX3 pm_ena: failed to import saved value {:#x}",
                value.pm_ena,
            ))
        })?;
        regs.pm_ctrl = PmCntrl::from_bits(value.pm_ctrl).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "PIIX3 pm_ctrl: failed to import saved value {:#x}",
                value.pm_ctrl,
            ))
        })?;
        Ok(regs)
    }
}

pub struct Piix3PM {
    pci_state: pci::DeviceState,

    /// ACPI PM Timer
    pub pmtimer: Arc<BhyvePmTimer>,

    regs: Mutex<PMRegs>,
    power_pin: Arc<dyn IntrPin>,
    log: slog::Logger,
}
impl Piix3PM {
    pub fn create(
        hdl: Arc<VmmHdl>,
        power_pin: Arc<dyn IntrPin>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let pci_state = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_INTEL,
            device_id: PIIX4_PM_DEV_ID,
            sub_vendor_id: VENDOR_OXIDE,
            sub_device_id: PIIX4_PM_SUB_DEV_ID,
            class: pci::bits::CLASS_BRIDGE,
            subclass: pci::bits::SUBCLASS_BRIDGE_OTHER,
            // Linux will complain about the PM-timer being potentially slow if
            // it detects the ACPI device exposing a revision prior to 0x3.
            revision_id: 0x3,
            ..Default::default()
        })
        .add_custom_cfg(PMCFG_OFFSET as u8, PMCFG_LEN as u8)
        // ACPI device requires lintr for SCI
        .add_lintr()
        .finish();

        let regs = PMRegs::default();
        Arc::new(Self {
            pci_state,

            pmtimer: BhyvePmTimer::create(hdl, regs.pmtimer_port()),

            regs: Mutex::new(regs),
            power_pin,
            log,
        })
    }

    pub fn attach(self: &Arc<Self>, pio: &PioBus) {
        // XXX: static registration for now
        let this = Arc::clone(&self);
        let piofn = Arc::new(move |port: u16, rwo: RWOp| this.pio_rw(port, rwo))
            as Arc<PioFn>;
        pio.register(PMBASE_DEFAULT, PMBASE_LEN, piofn).unwrap();
    }

    fn pio_rw(&self, _port: u16, mut rwo: RWOp) {
        PM_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmreg_read(id, ro),
            RWOp::Write(wo) => self.pmreg_write(id, wo),
        });
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
            PmCfg::SmbusBase => {
                // LSB hardwired to 1 to indicate PMBase in IO space
                ro.write_u32(SMBBASE_DEFAULT as u32 | 0x1);
            }
            PmCfg::DevResA => {
                ro.write_u32(DevResA::KBC_EN_DEV11.bits());
            }
            PmCfg::DevResB => {
                ro.write_u32(
                    (DevResB::KBC_EIO_EN | DevResB::EIO_EN_DEV5).bits(),
                );
            }
            PmCfg::DevResC => {
                ro.write_u32(
                    (DevResC::EIO_EN_DEV7
                        | DevResC::COMB_DEC_SEL_COM2
                        | DevResC::EIO_EN_DEV6
                        | DevResC::COMA_DEC_SEL_COM1)
                        .bits(),
                );
            }
            PmCfg::DevResD | PmCfg::DevResG => {
                ro.write_u16(0);
            }
            PmCfg::DevResE
            | PmCfg::DevResF
            | PmCfg::DevResH
            | PmCfg::DevResI
            | PmCfg::DevResJ => {
                ro.write_u32(0);
            }
            _ => {
                // XXX: report everything else as zeroed
                slog::info!(self.log, "piix3pm ignored cfg read";
                    "offset" => ro.offset(), "register" => ?id);
                ro.fill(0);
            }
        }
    }
    fn pmcfg_write(&self, id: &PmCfg, _wo: &WriteOp) {
        // XXX: ignore writes for now
        slog::info!(self.log, "piix3pm ignored cfg write";
            "offset" => _wo.offset(), "register" => ?id);
    }
    fn pmreg_read(&self, id: &PmReg, ro: &mut ReadOp) {
        let regs = &self.regs.lock().unwrap();
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
                slog::debug!(self.log, "piix3pm unhandled read";
                    "offset" => ro.offset(), "register" => ?id);
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
                        self.power_pin.pulse();
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
                slog::info!(self.log, "piix3pm unhandled write";
                    "offset" => wo.offset(), "register" => ?id);
            }
            PmReg::Reserved => {}
        }
    }
}
impl pci::Device for Piix3PM {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
    fn cfg_rw(&self, region: u8, mut rwo: RWOp) {
        assert_eq!(region as usize, PMCFG_OFFSET);

        PM_CFG_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.pmcfg_read(id, ro),
            RWOp::Write(wo) => self.pmcfg_write(id, wo),
        })
    }
}
impl Lifecycle for Piix3PM {
    fn type_name(&self) -> &'static str {
        "pci-piix3-pm"
    }
    fn reset(&self) {
        self.pci_state.reset(self);

        // Reset PM-specific registers.  If/when modifications to `pm_base` are
        // allowed, it will need to be more cognizant of the state inside the
        // BhyvePmTimer device.
        self.regs.lock().unwrap().reset();

        self.pmtimer.reset();
    }
    fn resume(&self) {
        self.pmtimer.resume();
    }
    fn start(&self) -> anyhow::Result<()> {
        self.pmtimer.start()
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for Piix3PM {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let regs = self.regs.lock().unwrap();
        output.push(Into::<migrate::Piix3PmV1>::into(*regs).into())?;

        MigrateMulti::export(&self.pci_state, output, ctx)?;

        MigrateMulti::export(self.pmtimer.as_ref(), output, ctx)?;

        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::Piix3PmV1 = offer.take()?;
        let xlated_regs: PMRegs = data.try_into()?;

        *self.regs.lock().unwrap() = xlated_regs;

        MigrateMulti::import(&self.pci_state, offer, ctx)?;

        MigrateMulti::import(self.pmtimer.as_ref(), offer, ctx)?;

        Ok(())
    }
}

mod migrate {
    use crate::migrate::*;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct I440FxHostBridgeV1 {
        pub pci_cfg_addr: u32,
    }
    impl Schema<'_> for I440FxHostBridgeV1 {
        fn id() -> SchemaId {
            ("i440fx-hb", 1)
        }
    }

    #[derive(Deserialize, Serialize)]
    pub struct Piix3LpcV1 {
        pub pir_regs: [u8; super::PIR_LEN],
        pub post_code: u8,
    }
    impl Schema<'_> for Piix3LpcV1 {
        fn id() -> SchemaId {
            ("piix3-lpc", 1)
        }
    }

    #[derive(Deserialize, Serialize)]
    pub struct Piix3PmV1 {
        pub pm_base: u16,
        pub pm_status: u16,
        pub pm_ena: u16,
        pub pm_ctrl: u16,
    }
    impl Schema<'_> for Piix3PmV1 {
        fn id() -> SchemaId {
            ("piix3-pm", 1)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::hw::pci::device::test::*;
    use crate::hw::pci::test::Scaffold;
    use crate::hw::pci::{self, Endpoint};
    use crate::intr_pins::NoOpPin;
    use crate::vmm::VmmHdl;

    use slog::{Discard, Logger};

    fn setup_attach(scaffold: &Scaffold, dev: Arc<dyn Endpoint>) -> pci::Bus {
        let bus = scaffold.create_bus();
        // just attach at slot 0 func 0
        bus.attach(pci::BusLocation::new(0, 0).unwrap(), dev, None);
        bus
    }

    fn topo_attach(topo: &pci::topology::Topology, dev: Arc<dyn Endpoint>) {
        topo.pci_attach(
            pci::topology::LogicalBusId(0),
            pci::BusLocation::new(0, 0).unwrap(),
            dev,
            None,
        )
        .unwrap();
    }

    #[test]
    fn hb_pci_cfg_read() {
        let scaffold = Scaffold::new();
        let topo = scaffold.basic_topo();

        let hb = I440FxHostBridge::create(
            topo.clone(),
            Opts { enable_pcie: false, power_pin: None, reset_pin: None },
        );
        topo_attach(&topo, hb.clone());

        cfg_read(hb.as_ref() as &dyn Endpoint);
    }

    #[test]
    fn hb_pci_cfg_write() {
        let scaffold = Scaffold::new();
        let topo = scaffold.basic_topo();

        let hb = I440FxHostBridge::create(
            topo.clone(),
            Opts { enable_pcie: false, power_pin: None, reset_pin: None },
        );
        topo_attach(&topo, hb.clone());

        cfg_write(hb.as_ref() as &dyn Endpoint);
    }

    #[test]
    fn lpc_pci_cfg_read() {
        let hdl = Arc::new(VmmHdl::new_test(0).unwrap());
        let scaffold = Scaffold::new();

        let lpc = Piix3Lpc::create(hdl);
        let _bus = setup_attach(&scaffold, lpc.clone());

        cfg_read(lpc.as_ref() as &dyn Endpoint);
    }

    #[test]
    fn lpc_pci_cfg_write() {
        let hdl = Arc::new(VmmHdl::new_test(0).unwrap());
        let scaffold = Scaffold::new();

        let lpc = Piix3Lpc::create(hdl);
        let _bus = setup_attach(&scaffold, lpc.clone());

        cfg_write(lpc.as_ref() as &dyn Endpoint);
    }

    #[test]
    fn pm_pci_cfg_read() {
        let hdl = Arc::new(VmmHdl::new_test(0).unwrap());
        let scaffold = Scaffold::new();
        let log = Logger::root(Discard, slog::o!());
        let power_pin = Arc::new(NoOpPin {});

        let pm = Piix3PM::create(hdl, power_pin, log);
        let _bus = setup_attach(&scaffold, pm.clone());

        cfg_read(pm.as_ref() as &dyn Endpoint);
    }

    #[test]
    fn pm_pci_cfg_write() {
        let hdl = Arc::new(VmmHdl::new_test(0).unwrap());
        let scaffold = Scaffold::new();
        let log = Logger::root(Discard, slog::o!());
        let power_pin = Arc::new(NoOpPin {});

        let pm = Piix3PM::create(hdl, power_pin, log);
        let _bus = setup_attach(&scaffold, pm.clone());

        cfg_write(pm.as_ref() as &dyn Endpoint);
    }
}
