// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ffi::c_void;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use super::probes;
use super::queue::{self, VirtQueues};
use super::{VirtioDevice, VirtioIntr, VqChange, VqIntr};
use crate::common::{RWOp, ReadOp, WriteOp, PAGE_SHIFT, PAGE_SIZE};
use crate::hw::pci::{self, BarN, CapId};
use crate::hw::virtio;
use crate::hw::virtio::queue::VqSize;
use crate::intr_pins::IntrPin;
use crate::migrate::{
    MigrateCtx, MigrateMulti, MigrateStateError, PayloadOffers, PayloadOutputs,
};
use crate::util::regmap::RegMap;

use bit_field::BitField;
use lazy_static::lazy_static;

const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;

const VIRTIO_PCI_ISR_QUEUE: u8 = 1 << 0;
const VIRTIO_PCI_ISR_CFG: u8 = 1 << 1;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Status: u8 {
        const RESET = 0;
        const ACK = 1 << 0;
        const DRIVER = 1 << 1;
        const DRIVER_OK = 1 << 2;
        const FEATURES_OK = 1 << 3;
        const NEEDS_RESET = 1 << 6;
        const FAILED = 1 << 7;
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum IntrMode {
    IsrOnly,
    IsrLintr,
    Msi,
}
impl From<pci::IntrMode> for IntrMode {
    fn from(pci_mode: pci::IntrMode) -> Self {
        match pci_mode {
            pci::IntrMode::Disabled => IntrMode::IsrOnly,
            pci::IntrMode::INTxPin => IntrMode::IsrLintr,
            pci::IntrMode::Msix => IntrMode::Msi,
        }
    }
}

struct VirtioState {
    status: Status,
    queue_select: u16,
    negotiated_features: u64,
    /// True if the high-half of feature register has been selected via
    /// the feature select register; false if the low-half is selected
    /// (which is always the case for legacy devices).
    device_feature_select: u32,
    driver_feature_select: u32,
    config_generation: u8,
    config_generation_seen: bool,
    device_config_size: usize,
    mode: virtio::Mode,
    intr_mode: IntrMode,
    intr_mode_updating: bool,
    msix_cfg_vec: u16,
    msix_queue_vec: Vec<u16>,
}

impl VirtioState {
    fn new(
        device_config_size: usize,
        nmsix: usize,
        mode: virtio::Mode,
    ) -> Self {
        let msix_queue_vec = vec![VIRTIO_MSI_NO_VECTOR; nmsix];
        Self {
            status: Status::RESET,
            queue_select: 0,
            negotiated_features: 0,
            device_feature_select: 0,
            driver_feature_select: 0,
            config_generation: 0,
            config_generation_seen: false,
            device_config_size,
            mode,
            intr_mode: IntrMode::IsrOnly,
            intr_mode_updating: false,
            msix_cfg_vec: VIRTIO_MSI_NO_VECTOR,
            msix_queue_vec,
        }
    }

    fn reset(&mut self) {
        self.status = Status::RESET;
        self.queue_select = 0;
        self.negotiated_features = 0;
        self.config_generation = 0;
        self.config_generation_seen = false;
        self.msix_cfg_vec = VIRTIO_MSI_NO_VECTOR;
        self.msix_queue_vec.fill(VIRTIO_MSI_NO_VECTOR);
    }

    fn witness_config_generation(&mut self) {
        self.config_generation_seen = true;
    }

    fn _evolve_config_generation(&mut self) {
        if self.config_generation_seen {
            self.config_generation = self.config_generation.wrapping_add(1);
            self.config_generation_seen = false;
        }
    }
}

pub trait PciVirtio: VirtioDevice + Send + Sync + 'static {
    fn virtio_state(&self) -> &PciVirtioState;
    fn pci_state(&self) -> &pci::DeviceState;

    /// Handles notification that the IO port representing the queue
    /// notification register in the device BAR has changed.
    fn notify_port_update(&self, state: Option<NonZeroU16>) {
        let _used = state;
    }

    /// Handles notification that an MMIO address in the range representing the
    /// queue notification register in the device BAR has changed.
    fn notify_mmio_addr_update(&self, addr: Option<u64>) {
        let _used = addr;
    }

    /// Handles notification from the PCI emulation layer that one of the BARs
    /// has undergone a configuration change.
    fn bar_update(&self, bstate: pci::BarState) {
        match bstate.id {
            pci::BarN::BAR0 => {
                // Notify the device about the location (if any) of the Queue
                // Notify register in the containing BAR region.
                let port = bstate.decode_en.then(|| {
                    // Having registered `bstate.value` as the address in BAR0
                    // only succeeds if that address through to the size of the
                    // registered region - the virtio legacy config registers -
                    // does not wrap. The base address *could* be zero, unwise
                    // as that would be, but adding LEGACY_REG_OFF_QUEUE_NOTIFY
                    // guarantees that the computed offset here is non-zero.
                    NonZeroU16::new(
                        bstate.value as u16
                            + LEGACY_REG_QUEUE_NOTIFY_OFFSET as u16,
                    )
                    .expect("addition does not wrap")
                });
                self.notify_port_update(port);
            }
            pci::BarN::BAR2 => {
                let addr = bstate
                    .decode_en
                    .then(|| bstate.value + NOTIFY_REG_OFFSET as u64);
                self.notify_mmio_addr_update(addr);
            }
            _ => {}
        }
    }
}

impl<D: PciVirtio + Send + Sync + 'static> pci::Device for D {
    fn device_state(&self) -> &pci::DeviceState {
        self.pci_state()
    }

    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp) {
        let vs = self.virtio_state();
        let map = match bar {
            pci::BarN::BAR0 => {
                if vs.legacy_map_use_msix.load(Ordering::SeqCst) {
                    &vs.legacy_config
                } else {
                    &vs.legacy_config_nomsix
                }
            }
            pci::BarN::BAR2 => &vs.common_config,
            _ => panic!("Config IO to unsupported BAR {bar:?}"),
        };
        map.process(&mut rwo, |id, mut rwo| match id {
            VirtioConfigRegBlock::Common => {
                COMMON_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.common_read(self, id, ro),
                    RWOp::Write(wo) => {
                        vs.common_write(self.pci_state(), self, id, wo)
                    }
                })
            }
            VirtioConfigRegBlock::Notify => {
                NOTIFY_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.notify_read(id, ro),
                    RWOp::Write(wo) => vs.notify_write(self, id, wo),
                })
            }
            VirtioConfigRegBlock::IsrStatus => {
                ISR_STATUS_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.isr_status_read(id, ro),
                    RWOp::Write(_wo) => {
                        // Read-only for device.
                    }
                })
            }
            VirtioConfigRegBlock::Legacy => {
                LEGACY_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.legacy_read(self, id, ro),
                    RWOp::Write(wo) => {
                        vs.legacy_write(self.pci_state(), self, id, wo)
                    }
                })
            }
            VirtioConfigRegBlock::DeviceConfig => self.rw_dev_config(rwo),
            // Write ignored, read as zero.
            VirtioConfigRegBlock::RazWi => {}
        });
    }

    fn cap_rw(&self, id: CapId<u32>, mut rwo: RWOp) {
        let vs = self.virtio_state();
        let id = {
            let CapId::Vendor(tag) = id else {
                unimplemented!("Unhandled capability type: {id:x?}");
            };
            let Ok(id) = VirtioCfgCapTag::try_from(tag) else {
                unimplemented!("Unknown vendor capability: {id:x?}");
            };
            id
        };
        match id {
            VirtioCfgCapTag::Common => {
                COMMON_CFG_CAP_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.common_cfg_cap_read(id, ro),
                    RWOp::Write(_) => {
                        // Read-only for driver
                    }
                });
            }
            VirtioCfgCapTag::Notify => {
                NOTIFY_CFG_CAP_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.notify_cfg_cap_read(id, ro),
                    RWOp::Write(_) => {
                        // Read-only for driver
                    }
                });
            }
            VirtioCfgCapTag::Isr => {
                COMMON_CFG_CAP_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.isr_cfg_cap_read(id, ro),
                    RWOp::Write(_) => {
                        // Read-only for driver
                    }
                });
            }
            VirtioCfgCapTag::Device => {
                COMMON_CFG_CAP_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.device_cfg_cap_read(id, ro),
                    RWOp::Write(_) => {
                        // Note: unlike most other hypervisors, Propolis does
                        // not presently support writes via the device config
                        // register. So, e.g., one cannot set a MAC address this
                        // way.
                        // TODO: Plumb a logging object through into here.
                        // error!(
                        //     self.log,
                        //     "unsupported write {wo:?} to dev config register"
                        // ),
                        eprintln!("unsupported write to device cap reg");
                    }
                })
            }
            VirtioCfgCapTag::Pci => {
                PCI_CFG_CAP_REGS.process(&mut rwo, |id, rwo| match rwo {
                    RWOp::Read(ro) => vs.pci_cfg_cap_read(self, id, ro),
                    RWOp::Write(wo) => vs.pci_cfg_cap_write(self, id, wo),
                });
            }
            VirtioCfgCapTag::SharedMemory => {
                unimplemented!("VirtIO Shared Memory is unsupported");
            }
            VirtioCfgCapTag::Vendor => {
                unimplemented!("VirtIO Vendor capabilities are unsupported");
            }
        }
    }

    fn attach(&self) {
        let ps = self.pci_state();
        if let Some(pin) = ps.lintr_pin() {
            let vs = self.virtio_state();
            vs.isr_state.set_pin(pin);
        }
    }

    fn interrupt_mode_change(&self, intr_mode: pci::IntrMode) {
        let vs = self.virtio_state();
        vs.set_intr_mode(self.pci_state(), intr_mode.into(), false);
    }

    fn msi_update(&self, info: pci::MsiUpdate) {
        let vs = self.virtio_state();
        let mut state = vs.state.lock().unwrap();
        if state.intr_mode != IntrMode::Msi {
            return;
        }
        state =
            vs.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();
        state.intr_mode_updating = true;

        for vq in vs.queues.iter() {
            let val = *state.msix_queue_vec.get(vq.id as usize).unwrap();

            // avoid deadlock while modify per-VQ interrupt config
            drop(state);

            let result = match info {
                pci::MsiUpdate::MaskAll | pci::MsiUpdate::UnmaskAll
                    if val != VIRTIO_MSI_NO_VECTOR =>
                {
                    self.queue_change(vq, VqChange::IntrCfg)
                }
                pci::MsiUpdate::Modify(idx) if val == idx => {
                    self.queue_change(vq, VqChange::IntrCfg)
                }
                _ => Ok(()),
            };

            state = vs.state.lock().unwrap();
            if result.is_err() {
                // An error updating the VQ interrupt config should set the
                // device in a failed state.
                vs.needs_reset_locked(self, &mut state);
            }
        }
        state.intr_mode_updating = false;
        vs.state_cv.notify_all();
    }

    fn bar_update(&self, bstate: pci::BarState) {
        PciVirtio::bar_update(self, bstate);
    }
}

pub struct PciVirtioState {
    pub queues: VirtQueues,

    state: Mutex<VirtioState>,
    state_cv: Condvar,
    isr_state: Arc<IsrState>,

    common_config: RegMap<VirtioConfigRegBlock>,

    legacy_config: RegMap<VirtioConfigRegBlock>,
    legacy_config_nomsix: RegMap<VirtioConfigRegBlock>,

    /// Quick access to register map for MSIX (true) or non-MSIX (false)
    legacy_map_use_msix: AtomicBool,
}

impl PciVirtioState {
    pub(super) fn new(
        mode: virtio::Mode,
        queues: VirtQueues,
        msix_count: Option<u16>,
        device_type: virtio::DeviceId,
        cfg_size: usize,
    ) -> (Self, pci::DeviceState) {
        assert!(cfg_size < PAGE_SIZE);
        assert!(cfg_size + LEGACY_REG_SIZE < 0x200);

        let ident = device_type.pci_ident(mode).expect("PCI Ident");
        let mut builder = pci::Builder::new(ident).add_lintr();

        if let Some(count) = msix_count {
            builder = builder.add_cap_msix(pci::BarN::BAR1, count);
        }

        if mode == virtio::Mode::Transitional || mode == virtio::Mode::Legacy {
            // XXX: properly size the legacy cfg BAR
            builder = builder.add_bar_io(pci::BarN::BAR0, 0x200);
        }
        if mode == virtio::Mode::Transitional || mode == virtio::Mode::Modern {
            builder =
                builder.add_bar_mmio(pci::BarN::BAR2, 4 * PAGE_SIZE as u32);
            builder = builder.add_cap_vendor(
                VirtioCfgCapTag::Common.into(),
                COMMON_CFG_CAP_SIZE,
            );
            // Note: we don't presently support a non-zero multiplier for the
            // notification register, so we don't need to size this for the
            // number of queues; hence the fixed size.
            builder = builder.add_cap_vendor(
                VirtioCfgCapTag::Notify.into(),
                NOTIFY_CFG_CAP_SIZE,
            );
            builder = builder.add_cap_vendor(
                VirtioCfgCapTag::Isr.into(),
                COMMON_CFG_CAP_SIZE,
            );
            builder = builder.add_cap_vendor(
                VirtioCfgCapTag::Device.into(),
                COMMON_CFG_CAP_SIZE,
            );
            builder = builder
                .add_cap_vendor(VirtioCfgCapTag::Pci.into(), PCI_CFG_CAP_SIZE);
        }
        let pci_state = builder.finish();

        // With respect to layout, for the time being, we are unconditionally
        // transitional, meaning that we support both the legacy and common
        // configuration layouts.
        let common_config = RegMap::create_packed_passthru(
            4 * PAGE_SIZE,
            &[
                (VirtioConfigRegBlock::Common, COMMON_REG_SIZE),
                (VirtioConfigRegBlock::RazWi, PAGE_SIZE - COMMON_REG_SIZE),
                (VirtioConfigRegBlock::DeviceConfig, cfg_size),
                (VirtioConfigRegBlock::RazWi, PAGE_SIZE - cfg_size),
                (VirtioConfigRegBlock::Notify, NOTIFY_REG_SIZE),
                (VirtioConfigRegBlock::RazWi, PAGE_SIZE - NOTIFY_REG_SIZE),
                (VirtioConfigRegBlock::IsrStatus, ISR_STATUS_REG_SIZE),
                (VirtioConfigRegBlock::RazWi, PAGE_SIZE - ISR_STATUS_REG_SIZE),
            ],
        );
        let legacy_config = RegMap::create_packed_passthru(
            cfg_size + LEGACY_REG_SIZE,
            &[
                (VirtioConfigRegBlock::Legacy, LEGACY_REG_SIZE),
                (VirtioConfigRegBlock::DeviceConfig, cfg_size),
            ],
        );
        let legacy_config_nomsix = RegMap::create_packed_passthru(
            cfg_size + LEGACY_REG_SIZE_NO_MSIX,
            &[
                (VirtioConfigRegBlock::Legacy, LEGACY_REG_SIZE_NO_MSIX),
                (VirtioConfigRegBlock::DeviceConfig, cfg_size),
            ],
        );
        let legacy_map_use_msix = AtomicBool::new(false);
        // Allow VQs to access memory through the PCI state
        let nmsix = queues.max_capacity();
        let state = Mutex::new(VirtioState::new(cfg_size, nmsix, mode));
        let state_cv = Condvar::new();
        let isr_state = IsrState::new();
        let this = Self {
            queues,
            state,
            state_cv,
            isr_state,
            common_config,
            legacy_config,
            legacy_config_nomsix,
            legacy_map_use_msix,
        };

        for queue in this.queues.iter() {
            queue.set_intr(IsrIntr::new(&this.isr_state));
            pci_state
                .acc_mem
                .adopt(&queue.acc_mem, Some(format!("VQ {}", queue.id)));
        }

        (this, pci_state)
    }

    pub fn mode(&self) -> virtio::Mode {
        self.state.lock().unwrap().mode
    }

    fn qaddr<F>(&self, queue_select: u16, thunk: F) -> u64
    where
        F: FnOnce(&virtio::queue::MapInfo) -> u64,
    {
        self.queues
            .get(queue_select)
            .map(|queue| {
                let state = queue.get_state();
                thunk(&state.mapping)
            })
            .unwrap_or(0)
    }

    fn common_read(
        &self,
        dev: &dyn VirtioDevice,
        id: &CommonConfigReg,
        ro: &mut ReadOp,
    ) {
        match id {
            CommonConfigReg::DeviceFeatureSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.device_feature_select);
            }
            CommonConfigReg::DeviceFeature => {
                let state = self.state.lock().unwrap();
                let shift = state.device_feature_select * 32;
                let features = if shift < 64 {
                    self.features_supported(dev) >> shift
                } else {
                    0
                };
                ro.write_u32(features as u32);
            }
            CommonConfigReg::DriverFeatureSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.driver_feature_select);
            }
            CommonConfigReg::DriverFeature => {
                let state = self.state.lock().unwrap();
                let shift = state.driver_feature_select * 32;
                let features = if shift < 64 {
                    state.negotiated_features >> shift
                } else {
                    0
                };
                ro.write_u32(features as u32);
            }
            CommonConfigReg::ConfigMsixVector => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.msix_cfg_vec);
            }
            CommonConfigReg::NumQueues => {
                ro.write_u16(self.queues.count().get());
            }
            CommonConfigReg::DeviceStatus => {
                let state = self.state.lock().unwrap();
                ro.write_u8(state.status.bits());
            }
            CommonConfigReg::ConfigGeneration => {
                let mut state = self.state.lock().unwrap();
                state.witness_config_generation();
                ro.write_u8(state.config_generation);
            }
            CommonConfigReg::QueueSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.queue_select);
            }
            CommonConfigReg::QueueSize => {
                let state = self.state.lock().unwrap();
                let size = self
                    .queues
                    .get(state.queue_select)
                    .map(|vq| vq.size())
                    .unwrap_or(0);
                ro.write_u16(size);
            }
            CommonConfigReg::QueueMsixVector => {
                let state = self.state.lock().unwrap();
                let vector = state
                    .msix_queue_vec
                    .get(state.queue_select as usize)
                    .map(|queue_sel| *queue_sel)
                    .unwrap_or(VIRTIO_MSI_NO_VECTOR);
                ro.write_u16(vector);
            }
            CommonConfigReg::QueueEnable => {
                let state = self.state.lock().unwrap();
                let enabled = self
                    .queues
                    .get(state.queue_select)
                    .map(|q| q.is_enabled())
                    .unwrap_or(false);
                ro.write_u16(enabled.into())
            }
            CommonConfigReg::QueueNotifyOffset => {
                ro.write_u16(0);
            }
            CommonConfigReg::QueueDescAddr => {
                let state = self.state.lock().unwrap();
                let addr = self.qaddr(state.queue_select, |m| m.desc_addr);
                ro.write_u64(addr);
            }
            CommonConfigReg::QueueDriverAddr => {
                let state = self.state.lock().unwrap();
                let addr = self.qaddr(state.queue_select, |m| m.avail_addr);
                ro.write_u64(addr);
            }
            CommonConfigReg::QueueDeviceAddr => {
                let state = self.state.lock().unwrap();
                let addr = self.qaddr(state.queue_select, |m| m.used_addr);
                ro.write_u64(addr);
            }
            // Note: currently unused.
            CommonConfigReg::QueueNotifyData => {
                let state = self.state.lock().unwrap();
                let data = self
                    .queues
                    .get(state.queue_select)
                    .map(|q| q.notify_data)
                    .unwrap_or(0);
                ro.write_u16(data);
            }
            // Note: currently unused.
            CommonConfigReg::QueueReset => {
                ro.write_u16(0);
            }
        }
    }

    fn common_write(
        &self,
        pci_state: &pci::DeviceState,
        dev: &dyn VirtioDevice,
        id: &CommonConfigReg,
        wo: &mut WriteOp,
    ) {
        match id {
            CommonConfigReg::DeviceFeatureSelect => {
                let mut state = self.state.lock().unwrap();
                state.device_feature_select = wo.read_u32();
            }
            CommonConfigReg::DeviceFeature => {
                // Read-only for driver
            }
            CommonConfigReg::DriverFeatureSelect => {
                let mut state = self.state.lock().unwrap();
                state.driver_feature_select = wo.read_u32();
            }
            CommonConfigReg::DriverFeature => {
                let mut state = self.state.lock().unwrap();
                let shift = state.driver_feature_select * 32;
                if shift < 64 {
                    let current = {
                        let lo = 32 - shift as usize;
                        let hi = 64 - shift as usize;
                        state.negotiated_features.get_bits(lo..hi) << lo
                    };
                    let offered = (u64::from(wo.read_u32()) << shift) | current;
                    let negotiated = self.features_supported(dev) & offered;
                    state.negotiated_features = negotiated;
                }
            }
            CommonConfigReg::ConfigMsixVector => {
                let mut state = self.state.lock().unwrap();
                state.msix_cfg_vec = wo.read_u16();
            }
            CommonConfigReg::NumQueues => {
                // Read-only for driver
            }
            CommonConfigReg::DeviceStatus => {
                self.set_status(dev, wo.read_u8());
            }
            CommonConfigReg::ConfigGeneration => {
                // Read-only for driver
            }
            CommonConfigReg::QueueSelect => {
                let mut state = self.state.lock().unwrap();
                state.queue_select = wo.read_u16();
            }
            CommonConfigReg::QueueSize => {
                let state = self.state.lock().unwrap();
                match VqSize::try_from(wo.read_u16()) {
                    Err(_) => {
                        // Bad queue size.
                        self.set_needs_reset(dev);
                    }
                    Ok(offered) => {
                        let qs = state.queue_select;
                        let Some(queue) = self.queues.get(qs) else {
                            // Invalid queue; write dropped.
                            return;
                        };
                        let mut size = queue.size.lock().unwrap();
                        *size = offered;
                    }
                }
            }
            CommonConfigReg::QueueMsixVector => {
                let hdl = pci_state.msix_hdl().unwrap();
                let mut state = self.state.lock().unwrap();
                let sel = state.queue_select as usize;
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let val = wo.read_u16();

                    if state.intr_mode != IntrMode::Msi {
                        // Store the vector information for later
                        state.msix_queue_vec[sel] = val;
                    } else {
                        state = self
                            .state_cv
                            .wait_while(state, |s| s.intr_mode_updating)
                            .unwrap();
                        state.intr_mode_updating = true;
                        state.msix_queue_vec[sel] = val;

                        // State lock cannot be held while updating queue
                        // interrupt handlers due to deadlock possibility.
                        drop(state);
                        queue.set_intr(MsiIntr::new(hdl, val));
                        state = self.state.lock().unwrap();

                        // With the MSI configuration updated for the virtqueue,
                        // notify the device of the change
                        if dev.queue_change(queue, VqChange::IntrCfg).is_err() {
                            self.needs_reset_locked(dev, &mut state);
                        }

                        state.intr_mode_updating = false;
                        self.state_cv.notify_all();
                    }
                }
            }
            CommonConfigReg::QueueEnable => {
                let mut state = self.state.lock().unwrap();
                let enabled = wo.read_u16() != 0;
                if let Some(queue) = self.queues.get(state.queue_select) {
                    if enabled {
                        queue.enable();
                        if dev.queue_change(queue, VqChange::Address).is_err() {
                            self.needs_reset_locked(dev, &mut state);
                        }
                    }
                }
            }
            CommonConfigReg::QueueNotifyOffset => {
                // Read-only for driver
            }
            CommonConfigReg::QueueDescAddr => {
                let state = self.state.lock().unwrap();
                let offered_desc_addr = wo.read_u64();
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let current = &queue.get_state().mapping;
                    queue.map_virtqueue(
                        offered_desc_addr,
                        current.avail_addr,
                        current.used_addr,
                    );
                }
            }
            CommonConfigReg::QueueDriverAddr => {
                let state = self.state.lock().unwrap();
                let offered_avail_addr = wo.read_u64();
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let current = &queue.get_state().mapping;
                    queue.map_virtqueue(
                        current.desc_addr,
                        offered_avail_addr,
                        current.used_addr,
                    );
                }
            }
            CommonConfigReg::QueueDeviceAddr => {
                let state = self.state.lock().unwrap();
                let offered_used_addr = wo.read_u64();
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let current = &queue.get_state().mapping;
                    queue.map_virtqueue(
                        current.desc_addr,
                        current.avail_addr,
                        offered_used_addr,
                    );
                }
            }
            CommonConfigReg::QueueNotifyData => {
                // Read-only for driver
            }
            // Note that this is a per-queue register, but since we don't
            // advertise the `VIRTIO_F_RING_RESET` feature bit, if we see
            // it, resetting the device isn't unreasonable.
            CommonConfigReg::QueueReset => self.set_needs_reset(dev),
        }
    }

    fn notify_read(&self, id: &NotifyReg, ro: &mut ReadOp) {
        match id {
            NotifyReg::Notify => {
                ro.write_u16(0);
            }
        }
    }

    fn notify_write(
        &self,
        dev: &dyn VirtioDevice,
        id: &NotifyReg,
        wo: &mut WriteOp,
    ) {
        match id {
            NotifyReg::Notify => self.queue_notify(dev, wo.read_u16()),
        }
    }

    fn isr_status_read(&self, id: &IsrStatusReg, ro: &mut ReadOp) {
        match id {
            IsrStatusReg::IsrStatus => {
                // reading ISR Status clears it as well
                let isr = self.isr_state.read_clear();
                ro.write_u8(isr);
            }
        }
    }

    fn common_cfg_cap_read(&self, id: &CommonCfgCapReg, op: &mut ReadOp) {
        match id {
            CommonCfgCapReg::CapLen => op.write_u8(COMMON_CFG_CAP_SIZE + 2),
            CommonCfgCapReg::CfgType => {
                op.write_u8(VirtioCfgCapTag::Common as u8)
            }
            CommonCfgCapReg::Bar => op.write_u8(BarN::BAR2 as u8),
            CommonCfgCapReg::Id => op.write_u8(0),
            CommonCfgCapReg::Padding => {}
            CommonCfgCapReg::Offset => op.write_u32(COMMON_REG_OFFSET as u32),
            CommonCfgCapReg::Length => op.write_u32(COMMON_REG_SIZE as u32),
        }
    }

    fn notify_cfg_cap_read(&self, id: &NotifyCfgCapReg, op: &mut ReadOp) {
        match id {
            NotifyCfgCapReg::Common(common_id) => match common_id {
                CommonCfgCapReg::CfgType => {
                    op.write_u8(VirtioCfgCapTag::Notify as u8)
                }
                CommonCfgCapReg::CapLen => op.write_u8(NOTIFY_CFG_CAP_SIZE + 2),
                CommonCfgCapReg::Offset => {
                    op.write_u32(NOTIFY_REG_OFFSET as u32)
                }
                CommonCfgCapReg::Length => op.write_u32(NOTIFY_REG_SIZE as u32),
                _ => self.common_cfg_cap_read(common_id, op),
            },
            NotifyCfgCapReg::Multiplier => op.write_u32(0),
        }
    }

    fn device_cfg_cap_read(&self, id: &CommonCfgCapReg, op: &mut ReadOp) {
        match id {
            CommonCfgCapReg::CfgType => {
                op.write_u8(VirtioCfgCapTag::Device as u8)
            }
            CommonCfgCapReg::Offset => op.write_u32(DEVICE_REG_OFFSET as u32),
            CommonCfgCapReg::Length => {
                let state = self.state.lock().unwrap();
                op.write_u32(state.device_config_size as u32);
            }
            _ => self.common_cfg_cap_read(id, op),
        }
    }

    fn isr_cfg_cap_read(&self, id: &CommonCfgCapReg, op: &mut ReadOp) {
        match id {
            CommonCfgCapReg::CfgType => op.write_u8(VirtioCfgCapTag::Isr as u8),
            CommonCfgCapReg::Offset => {
                op.write_u32(ISR_STATUS_REG_OFFSET as u32)
            }
            CommonCfgCapReg::Length => op.write_u32(ISR_STATUS_REG_SIZE as u32),
            _ => self.common_cfg_cap_read(id, op),
        }
    }

    fn pci_cfg_cap_read(
        &self,
        dev: &dyn VirtioDevice,
        id: &PciCfgCapReg,
        op: &mut ReadOp,
    ) {
        let _todo = dev;
        match id {
            PciCfgCapReg::Common(common_id) => match common_id {
                CommonCfgCapReg::CfgType => {
                    op.write_u8(VirtioCfgCapTag::Pci as u8)
                }
                CommonCfgCapReg::Bar => op.write_u8(0), // TODO: Handle
                CommonCfgCapReg::Offset => op.write_u32(0), // TODO: Handle
                CommonCfgCapReg::Length => op.write_u32(0), // TODO: Handle
                _ => self.common_cfg_cap_read(common_id, op),
            },
            PciCfgCapReg::PciData => {
                // TODO: We actually need to handle this.
                op.write_u32(0);
            }
        }
    }

    fn pci_cfg_cap_write(
        &self,
        dev: &dyn VirtioDevice,
        id: &PciCfgCapReg,
        op: &mut WriteOp,
    ) {
        let _todo = (dev, op);
        match id {
            PciCfgCapReg::Common(common_id) => {
                match common_id {
                    CommonCfgCapReg::Bar => {
                        // TODO: Store the bar
                    }
                    CommonCfgCapReg::Offset => {
                        // TODO: Store the offset
                    }
                    CommonCfgCapReg::Length => {
                        // TODO: Store the length
                    }
                    // Everything else is read-only for the driver.
                    _ => {}
                }
            }
            PciCfgCapReg::PciData => {
                // TODO: Handle the write.
            }
        }
    }

    fn legacy_read(
        &self,
        dev: &dyn VirtioDevice,
        id: &LegacyConfigReg,
        ro: &mut ReadOp,
    ) {
        match id {
            LegacyConfigReg::DeviceFeature => {
                ro.write_u32(self.features_supported(dev) as u32);
            }
            LegacyConfigReg::DriverFeature => {
                let state = self.state.lock().unwrap();
                ro.write_u32(state.negotiated_features as u32);
            }
            LegacyConfigReg::QueueAddress4k => {
                let state = self.state.lock().unwrap();
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let qs = queue.get_state();
                    let addr = qs.mapping.desc_addr;
                    ro.write_u32((addr >> PAGE_SHIFT) as u32);
                } else {
                    // bogus queue
                    ro.write_u32(0);
                }
            }
            LegacyConfigReg::QueueSize => {
                let state = self.state.lock().unwrap();
                let sz = self
                    .queues
                    .get(state.queue_select)
                    .map(|vq| vq.size())
                    .unwrap_or(0);
                ro.write_u16(sz);
            }
            LegacyConfigReg::QueueSelect => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.queue_select);
            }
            LegacyConfigReg::QueueNotify => {}
            LegacyConfigReg::DeviceStatus => {
                let state = self.state.lock().unwrap();
                ro.write_u8(state.status.bits());
            }
            LegacyConfigReg::IsrStatus => {
                // reading ISR Status clears it as well
                let isr = self.isr_state.read_clear();
                ro.write_u8(isr);
            }
            LegacyConfigReg::ConfigMsixVector => {
                let state = self.state.lock().unwrap();
                ro.write_u16(state.msix_cfg_vec);
            }
            LegacyConfigReg::QueueMsixVector => {
                let state = self.state.lock().unwrap();
                let val = state
                    .msix_queue_vec
                    .get(state.queue_select as usize)
                    .map(|queue_sel| *queue_sel)
                    .unwrap_or(VIRTIO_MSI_NO_VECTOR);
                ro.write_u16(val);
            }
        }
    }

    fn legacy_write(
        &self,
        pci_state: &pci::DeviceState,
        dev: &dyn VirtioDevice,
        id: &LegacyConfigReg,
        wo: &mut WriteOp,
    ) {
        match id {
            LegacyConfigReg::DriverFeature => {
                let offered = u64::from(wo.read_u32());
                let negotiated = self.features_supported(dev) & offered;
                let mut state = self.state.lock().unwrap();
                match dev.set_features(negotiated) {
                    Ok(_) => {
                        state.negotiated_features = negotiated;
                    }
                    Err(_) => {
                        self.needs_reset_locked(dev, &mut state);
                    }
                }
            }
            LegacyConfigReg::QueueAddress4k => {
                let mut state = self.state.lock().unwrap();
                let pfn = wo.read_u32();
                if pfn == 0 {
                    return;
                }
                if let Some(queue) = self.queues.get(state.queue_select) {
                    let qs_old = queue.get_state();
                    let new_addr = u64::from(pfn) << PAGE_SHIFT;
                    queue.map_legacy(new_addr);
                    if qs_old.mapping.desc_addr != new_addr {
                        if dev.queue_change(queue, VqChange::Address).is_err() {
                            self.needs_reset_locked(dev, &mut state);
                        }
                    }
                }
            }
            LegacyConfigReg::QueueSelect => {
                self.common_write(
                    pci_state,
                    dev,
                    &CommonConfigReg::QueueSelect,
                    wo,
                );
            }
            LegacyConfigReg::QueueNotify => {
                self.queue_notify(dev, wo.read_u16());
            }
            LegacyConfigReg::DeviceStatus => {
                self.set_status(dev, wo.read_u8());
            }
            LegacyConfigReg::ConfigMsixVector => {
                let mut state = self.state.lock().unwrap();
                state.msix_cfg_vec = wo.read_u16();
            }
            LegacyConfigReg::QueueMsixVector => {
                self.common_write(
                    pci_state,
                    dev,
                    &CommonConfigReg::QueueMsixVector,
                    wo,
                );
            }

            LegacyConfigReg::DeviceFeature
            | LegacyConfigReg::QueueSize
            | LegacyConfigReg::IsrStatus => {
                // Read-only regs
            }
        }
    }

    fn features_supported(&self, dev: &dyn VirtioDevice) -> u64 {
        dev.features() | queue::Features::transitional().bits()
    }

    fn set_status(&self, dev: &dyn VirtioDevice, value: u8) {
        let mut state = self.state.lock().unwrap();
        let status = Status::from_bits_truncate(value);
        if status == Status::RESET && state.status != Status::RESET {
            self.virtio_reset(dev, state);
        } else {
            // XXX: better device status FSM
            state.status = status;
            if status.contains(Status::FEATURES_OK) {
                if dev.set_features(state.negotiated_features).is_err() {
                    self.needs_reset_locked(dev, &mut state);
                }
            }
        }
    }

    /// Set the "Needs Reset" state on the VirtIO device
    fn needs_reset_locked(
        &self,
        _dev: &dyn VirtioDevice,
        state: &mut MutexGuard<VirtioState>,
    ) {
        if !state.status.contains(Status::NEEDS_RESET) {
            state.status.insert(Status::NEEDS_RESET);
            // XXX: interrupt needed?
        }
    }

    /// Indicate to the guest that the VirtIO device has encountered an error of
    /// some sort and requires a reset.
    pub fn set_needs_reset(&self, dev: &dyn VirtioDevice) {
        let mut state = self.state.lock().unwrap();
        self.needs_reset_locked(dev, &mut state);
    }

    fn queue_notify(&self, dev: &dyn VirtioDevice, queue: u16) {
        probes::virtio_vq_notify!(|| (
            dev as *const dyn VirtioDevice as *const c_void as u64,
            queue
        ));
        if let Some(vq) = self.queues.get(queue) {
            vq.arise();
            if self.mode() != virtio::Mode::Modern || vq.is_enabled() {
                dev.queue_notify(vq);
            }
        }
    }

    /// Reset all non-control queues as part of a device reset (or shutdown).
    pub fn reset_queues(&self, dev: &dyn VirtioDevice) {
        let mut state = self.state.lock().unwrap();
        self.reset_queues_locked(dev, &mut state);
    }

    fn reset_queues_locked(
        &self,
        dev: &dyn VirtioDevice,
        state: &mut MutexGuard<VirtioState>,
    ) {
        for queue in self.queues.iter_all() {
            queue.reset();
            if dev.queue_change(queue, VqChange::Reset).is_err() {
                self.needs_reset_locked(dev, state);
            }
        }
    }

    /// Reset the virtio portion of the device
    ///
    /// This leaves PCI state (such as configured BARs) unchanged
    fn virtio_reset(
        &self,
        dev: &dyn VirtioDevice,
        mut state: MutexGuard<VirtioState>,
    ) {
        self.reset_queues_locked(dev, &mut state);
        state.reset();
        let _ = self.isr_state.read_clear();
    }

    pub fn reset<D>(&self, dev: &D)
    where
        D: pci::Device + PciVirtio,
    {
        let vs = dev.virtio_state();
        let ps = dev.pci_state();

        let state = vs.state.lock().unwrap();
        vs.virtio_reset(dev, state);
        ps.reset(dev);
    }

    fn set_intr_mode(
        &self,
        pci_state: &pci::DeviceState,
        new_mode: IntrMode,
        is_import: bool,
    ) {
        let mut state = self.state.lock().unwrap();
        let old_mode = state.intr_mode;
        if new_mode == old_mode {
            return;
        }

        state =
            self.state_cv.wait_while(state, |s| s.intr_mode_updating).unwrap();

        state.intr_mode_updating = true;
        match old_mode {
            IntrMode::IsrLintr => {
                // When leaving lintr-pin mode, deassert anything on said pin
                self.isr_state.disable();
            }
            IntrMode::Msi => {
                // When leaving MSI mode, re-wire the Isr interrupt handling
                //
                // To avoid deadlock, the state lock must be dropped while
                // updating the interrupts handlers on queues.
                drop(state);
                for queue in self.queues.iter() {
                    queue.set_intr(IsrIntr::new(&self.isr_state));
                }
                state = self.state.lock().unwrap();
            }
            _ => {}
        }

        state.intr_mode = new_mode;
        // Make sure the correct legacy register map is used
        self.legacy_map_use_msix
            .store(new_mode == IntrMode::Msi, Ordering::SeqCst);
        match new_mode {
            IntrMode::IsrLintr => {
                self.isr_state.enable(is_import);
            }
            IntrMode::Msi => {
                let hdl = pci_state.msix_hdl().unwrap();
                for vq in self.queues.iter() {
                    let vec = *state
                        .msix_queue_vec
                        .get(vq.id as usize)
                        .expect("msix for virtqueue is ok");

                    // State lock cannot be held while updating queue interrupt
                    // handlers due to deadlock possibility.
                    drop(state);
                    vq.set_intr(MsiIntr::new(hdl.clone(), vec));
                    state = self.state.lock().unwrap();
                }
            }
            _ => {}
        }
        state.intr_mode_updating = false;
        self.state_cv.notify_all();
    }

    pub fn negotiated_features(&self) -> u64 {
        let state = self.state.lock().unwrap();
        state.negotiated_features
    }
}
impl MigrateMulti for PciVirtioState {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let state = self.state.lock().unwrap();
        let (isr_queue, isr_cfg) = self.isr_state.read();

        let device = migrate::DeviceStateV1 {
            status: state.status.bits(),
            queue_select: state.queue_select,
            negotiated_features: state.negotiated_features,
            device_feature_select: state.device_feature_select,
            driver_feature_select: state.driver_feature_select,
            config_generation: state.config_generation,
            config_generation_seen: state.config_generation_seen,
            device_config_size: state.device_config_size as u64,
            mode: state.mode as u32,
            msix_cfg_vec: state.msix_cfg_vec,
            msix_queue_vec: state.msix_queue_vec.clone(),
            isr_queue,
            isr_cfg,
        };
        drop(state);

        let queues = self.queues.export();

        output.push(migrate::PciVirtioStateV1 { device, queues }.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let input: migrate::PciVirtioStateV1 = offer.take()?;

        let dev = input.device;
        let mut state = self.state.lock().unwrap();
        state.status = Status::from_bits(dev.status).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "virtio status: failed to import saved value {status:#x}",
                status = dev.status
            ))
        })?;
        state.queue_select = dev.queue_select;
        state.negotiated_features = dev.negotiated_features;
        state.device_feature_select = dev.device_feature_select;
        state.driver_feature_select = dev.driver_feature_select;
        state.config_generation = dev.config_generation;
        state.config_generation_seen = dev.config_generation_seen;
        state.device_config_size = dev.device_config_size as usize;
        state.mode = virtio::Mode::from_repr(dev.mode).ok_or_else(|| {
            MigrateStateError::ImportFailed(format!(
                "virtio mode: failed to import saved value {mode:#x}",
                mode = dev.mode
            ))
        })?;
        state.msix_cfg_vec = dev.msix_cfg_vec;
        state.msix_queue_vec = dev.msix_queue_vec;
        self.isr_state.import(dev.isr_queue, dev.isr_cfg);

        self.queues.import(&input.queues)?;

        Ok(())
    }
}

impl MigrateMulti for dyn PciVirtio {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let ps = self.pci_state();
        let vs = self.virtio_state();

        MigrateMulti::export(vs, output, ctx)?;
        MigrateMulti::export(ps, output, ctx)?;
        Ok(())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let ps = self.pci_state();
        let vs = self.virtio_state();

        MigrateMulti::import(vs, offer, ctx)?;
        MigrateMulti::import(ps, offer, ctx)?;

        // Now that PCI state is populated, apply its calculated interrupt mode
        // to the VirtIO state.
        vs.set_intr_mode(ps, ps.get_intr_mode().into(), true);

        // Perform a (potentially spurious) update notification for the BARs
        // containing the virtio registers.  This ensures that anything
        // interested in the placement of those BARs (such as the notify
        // logic) is configured properly.
        if let Some(bar0) = ps.bar(pci::BarN::BAR0) {
            self.bar_update(bar0);
        }
        if let Some(bar2) = ps.bar(pci::BarN::BAR2) {
            self.bar_update(bar2);
        }

        Ok(())
    }
}

#[derive(Default)]
struct IsrInner {
    disabled: bool,
    /// Is an ISR asserted for virtqueue(s) in this device?
    intr_queue: bool,
    /// Is an ISR asserted for a config change in this device?
    intr_cfg: bool,
    pin: Option<Arc<dyn IntrPin>>,
}
impl IsrInner {
    fn raised(&self) -> bool {
        self.intr_queue || self.intr_cfg
    }
}
struct IsrState(Mutex<IsrInner>);
impl IsrState {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(IsrInner::default())))
    }
    /// Raise queue ISR condition
    fn raise_queue(&self) {
        self.sync_pin(|inner| {
            inner.intr_queue = true;
        });
    }
    /// Read ISR value, then clear it.
    fn read_clear(&self) -> u8 {
        let (mut queue, mut cfg) = (false, false);
        self.sync_pin(|inner| {
            queue = inner.intr_queue;
            cfg = inner.intr_cfg;
            inner.intr_queue = false;
            inner.intr_cfg = false;
        });
        let mut val = 0;
        if queue {
            val |= VIRTIO_PCI_ISR_QUEUE;
        }
        if cfg {
            val |= VIRTIO_PCI_ISR_CFG;
        }
        val
    }
    /// Read ISR value.  Returns (`intr_queue`, `intr_cfg`)
    fn read(&self) -> (bool, bool) {
        let inner = self.0.lock().unwrap();
        (inner.intr_queue, inner.intr_cfg)
    }
    /// Import ISR value
    ///
    /// Sets the internal ISR value without propagating the state to the
    /// underlying pin, as is necessary when doing a migration related import of
    /// various device states.
    fn import(&self, intr_queue: bool, intr_cfg: bool) {
        let mut inner = self.0.lock().unwrap();
        inner.intr_queue = intr_queue;
        inner.intr_cfg = intr_cfg;
        if let Some(pin) = inner.pin.as_ref() {
            pin.import_state(inner.raised());
        }
    }
    /// Sync ISR state with any associated interrupt pin
    fn sync_pin(&self, f: impl FnOnce(&mut IsrInner)) {
        let mut inner = self.0.lock().unwrap();

        let raised_before = inner.raised();
        f(&mut inner);
        let raised_after = inner.raised();

        // Sync pin state with ISR value
        if !inner.disabled {
            if !raised_before && raised_after {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.assert()
                }
            }
            if raised_before && !raised_after {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.deassert()
                }
            }
        }
    }
    /// Disable state emission via interrupt pin
    fn disable(&self) {
        let mut inner = self.0.lock().unwrap();
        if !inner.disabled {
            if let Some(pin) = inner.pin.as_ref() {
                pin.deassert();
            }
            inner.disabled = true;
        }
    }
    /// Enable state emission via interrupt pin
    fn enable(&self, is_import: bool) {
        let mut inner = self.0.lock().unwrap();
        if inner.disabled {
            if inner.raised() && !is_import {
                if let Some(pin) = inner.pin.as_ref() {
                    pin.assert();
                }
            }
            inner.disabled = false;
        }
    }
    /// Set underlying interrupt pin.
    ///
    /// # Panics
    /// If called more than once on a given [IsrState]
    fn set_pin(&self, pin: Arc<dyn IntrPin>) {
        let mut inner = self.0.lock().unwrap();
        let old = inner.pin.replace(pin);
        // Loosen this in the future if/when PCI device attachment logic becomes
        // more sophisticated.
        assert!(old.is_none(), "set_pin() should not be called more than once");
    }
}

struct IsrIntr(Weak<IsrState>);
impl IsrIntr {
    fn new(state: &Arc<IsrState>) -> Box<Self> {
        Box::new(Self(Arc::downgrade(state)))
    }
}
impl VirtioIntr for IsrIntr {
    fn notify(&self) {
        if let Some(state) = Weak::upgrade(&self.0) {
            state.raise_queue()
        }
    }
    fn read(&self) -> VqIntr {
        VqIntr::Pin
    }
}

struct MsiIntr {
    hdl: pci::MsixHdl,
    index: u16,
}
impl MsiIntr {
    fn new(hdl: pci::MsixHdl, index: u16) -> Box<Self> {
        Box::new(Self { hdl, index })
    }
}
impl VirtioIntr for MsiIntr {
    fn notify(&self) {
        if self.index < self.hdl.count() {
            self.hdl.fire(self.index);
        }
    }
    fn read(&self) -> VqIntr {
        if self.index < self.hdl.count() {
            let data = self.hdl.read(self.index);
            VqIntr::Msi(data.addr, data.data, data.masked)
        } else {
            VqIntr::Pin
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum VirtioConfigRegBlock {
    Legacy,
    Common,
    DeviceConfig,
    Notify,
    IsrStatus,
    RazWi,
}

// Some of these sizes are drawn from the VirtIO specification (e.g., the
// sum for `COMMON_REG_SIZE` is the sum of the sizes of the data that make
// up the common register fields as defined in VirtIO 1.2).
//
// Others are somewhat abitrary; the page offsets we have chosen, for example,
// are of our own selection and are defined so that a guest driver can map
// different registers in their own pages (using 4KiB page mappings).  This is
// not strictly necessary, however.
const LEGACY_REG_SIZE: usize = 0x18;
const LEGACY_REG_SIZE_NO_MSIX: usize = LEGACY_REG_SIZE - 2 * 2;
const LEGACY_REG_QUEUE_NOTIFY_OFFSET: usize = 0x10;

const COMMON_REG_OFFSET: usize = 0;
const COMMON_REG_SIZE: usize =
    4 + 4 + 4 + 4 + 2 + 2 + 1 + 1 + 2 + 2 + 2 + 2 + 2 + 8 + 8 + 8 + 2 + 2;
const DEVICE_REG_OFFSET: usize = PAGE_SIZE;
const NOTIFY_REG_OFFSET: usize = 2 * PAGE_SIZE;
pub const NOTIFY_REG_SIZE: usize = 4;
const ISR_STATUS_REG_OFFSET: usize = 3 * PAGE_SIZE;
const ISR_STATUS_REG_SIZE: usize = 1;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CommonConfigReg {
    /// Configuration data for the device as a whole.
    DeviceFeatureSelect,
    DeviceFeature,
    DriverFeatureSelect,
    DriverFeature,
    ConfigMsixVector,
    NumQueues,
    DeviceStatus,
    ConfigGeneration,

    /// Configuration information for a specific queue.
    QueueSelect,
    QueueSize,
    QueueMsixVector,
    QueueEnable,
    QueueNotifyOffset,
    QueueDescAddr,
    QueueDriverAddr,
    QueueDeviceAddr,
    QueueNotifyData,
    QueueReset,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NotifyReg {
    Notify,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum IsrStatusReg {
    IsrStatus,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum LegacyConfigReg {
    DeviceFeature,
    DriverFeature,
    QueueAddress4k,
    QueueSize,
    QueueSelect,
    QueueNotify,
    DeviceStatus,
    IsrStatus,
    ConfigMsixVector,
    QueueMsixVector,
}

lazy_static! {
    static ref COMMON_REGS: RegMap<CommonConfigReg> = {
        let layout = [
            // These refer to the device as a whole.
            (CommonConfigReg::DeviceFeatureSelect, 4),
            (CommonConfigReg::DeviceFeature, 4),
            (CommonConfigReg::DriverFeatureSelect, 4),
            (CommonConfigReg::DriverFeature, 4),
            (CommonConfigReg::ConfigMsixVector, 2),
            (CommonConfigReg::NumQueues, 2),
            (CommonConfigReg::DeviceStatus, 1),
            (CommonConfigReg::ConfigGeneration, 1),
            // These are banked for specific virtqueues, distinguished
            // via the "QueueSelect" register.
            (CommonConfigReg::QueueSelect, 2),
            (CommonConfigReg::QueueSize, 2),
            (CommonConfigReg::QueueMsixVector, 2),
            (CommonConfigReg::QueueEnable, 2),
            (CommonConfigReg::QueueNotifyOffset, 2),
            (CommonConfigReg::QueueDescAddr, 8),
            (CommonConfigReg::QueueDriverAddr, 8),
            (CommonConfigReg::QueueDeviceAddr, 8),
            (CommonConfigReg::QueueNotifyData, 2),
            (CommonConfigReg::QueueReset, 2),
        ];
        RegMap::create_packed(COMMON_REG_SIZE, &layout, None)
    };

    static ref NOTIFY_REGS: RegMap<NotifyReg> = {
        let layout = [
            (NotifyReg::Notify, 4),
        ];
        RegMap::create_packed(NOTIFY_REG_SIZE, &layout, None)
    };

    static ref ISR_STATUS_REGS: RegMap<IsrStatusReg> = {
        let layout = [
            (IsrStatusReg::IsrStatus, 1),
        ];
        RegMap::create_packed(ISR_STATUS_REG_SIZE, &layout, None)
    };

    static ref LEGACY_REGS: RegMap<LegacyConfigReg> = {
        let layout = [
            (LegacyConfigReg::DeviceFeature, 4),
            (LegacyConfigReg::DriverFeature, 4),
            (LegacyConfigReg::QueueAddress4k, 4),
            (LegacyConfigReg::QueueSize, 2),
            (LegacyConfigReg::QueueSelect, 2),
            (LegacyConfigReg::QueueNotify, 2),
            (LegacyConfigReg::DeviceStatus, 1),
            (LegacyConfigReg::IsrStatus, 1),
            (LegacyConfigReg::ConfigMsixVector, 2),
            (LegacyConfigReg::QueueMsixVector, 2),
        ];
        RegMap::create_packed(LEGACY_REG_SIZE, &layout, None)
    };
}

/// VirtIO configuration capabilities.
///
/// These definitions come from the description of
/// `cfg_type` in section 4.1.4 in VirtIO 1.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
enum VirtioCfgCapTag {
    Common = 1,
    Notify = 2,
    Isr = 3,
    Device = 4,
    Pci = 5,
    SharedMemory = 8,
    Vendor = 9,
}

impl From<VirtioCfgCapTag> for u32 {
    fn from(tag: VirtioCfgCapTag) -> u32 {
        tag as u32
    }
}

impl TryFrom<u32> for VirtioCfgCapTag {
    type Error = u32;
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        match raw {
            1 => Ok(Self::Common),
            2 => Ok(Self::Notify),
            3 => Ok(Self::Isr),
            4 => Ok(Self::Device),
            5 => Ok(Self::Pci),
            8 => Ok(Self::SharedMemory),
            9 => Ok(Self::Vendor),
            _ => Err(raw),
        }
    }
}

const COMMON_CFG_CAP_SIZE: u8 = 1 + 1 + 1 + 1 + 2 + 4 + 4;
const NOTIFY_CFG_CAP_SIZE: u8 = COMMON_CFG_CAP_SIZE + 4;
const PCI_CFG_CAP_SIZE: u8 = COMMON_CFG_CAP_SIZE + 4;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CommonCfgCapReg {
    CapLen,
    CfgType,
    Bar,
    Id,
    Padding,
    Offset,
    Length,
}

lazy_static! {
    /// The common configuration capability registers in config space.  Note
    /// that the capability type and next pointer are not included here, as
    /// these are defined and consumed by the framework.  So while the padding
    /// field appears to pad to a 6 byte offset, it actually pads to an 8 byte
    /// offset, as the entire register space is already offset by two bytes.
    ///
    /// This definition corresponds to `struct virtio_pci_cap` from sec 4.1.4
    /// of VirtIO 1.2.
    static ref COMMON_CFG_CAP_REGS: RegMap<CommonCfgCapReg> = {
        let layout = [
            (CommonCfgCapReg::CapLen, 1),
            (CommonCfgCapReg::CfgType, 1),
            (CommonCfgCapReg::Bar, 1),
            (CommonCfgCapReg::Id, 1),
            (CommonCfgCapReg::Padding, 2),  // Note, includes type and next
            (CommonCfgCapReg::Offset, 4),
            (CommonCfgCapReg::Length, 4),
        ];
        RegMap::create_packed(COMMON_CFG_CAP_SIZE.into(), &layout, None)
    };
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NotifyCfgCapReg {
    Common(CommonCfgCapReg),
    Multiplier,
}

lazy_static! {
    /// The nofiticiation capability regsiters in config space.
    ///
    /// See the note around `COMMON_CFG_CAP_REGS` for details about
    /// padding, offsets, and alignment.  This definition corresponds
    /// to `struct virtio_pci_notify_cap` from sec 4.1.4.4 of VirtIO 1.2.
    static ref NOTIFY_CFG_CAP_REGS: RegMap<NotifyCfgCapReg> = {
        let layout = [
            (NotifyCfgCapReg::Common(CommonCfgCapReg::CapLen), 1),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::CfgType), 1),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::Bar), 1),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::Id), 1),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::Padding), 2),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::Offset), 4),
            (NotifyCfgCapReg::Common(CommonCfgCapReg::Length), 4),
            (NotifyCfgCapReg::Multiplier, 4),
        ];
        RegMap::create_packed(NOTIFY_CFG_CAP_SIZE.into(), &layout, None)
    };
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PciCfgCapReg {
    Common(CommonCfgCapReg),
    PciData,
}

lazy_static! {
    /// The PCI configuration capability register in config space.
    ///
    /// See the note around `COMMON_CFG_CAP_REGS` for details about
    /// padding, offsets, and alignment.  This definition corresponds
    /// to `struct virtio_pci_cfg_cap` from sec 4.1.4.9 of VirtIO 1.2.
    static ref PCI_CFG_CAP_REGS: RegMap<PciCfgCapReg> = {
        let layout = [
            (PciCfgCapReg::Common(CommonCfgCapReg::CapLen), 1),
            (PciCfgCapReg::Common(CommonCfgCapReg::CfgType), 1),
            (PciCfgCapReg::Common(CommonCfgCapReg::Bar), 1),
            (PciCfgCapReg::Common(CommonCfgCapReg::Id), 1),
            (PciCfgCapReg::Common(CommonCfgCapReg::Padding), 2),
            (PciCfgCapReg::Common(CommonCfgCapReg::Offset), 4),
            (PciCfgCapReg::Common(CommonCfgCapReg::Length), 4),
            (PciCfgCapReg::PciData, 4),
        ];
        RegMap::create_packed(PCI_CFG_CAP_SIZE.into(), &layout, None)
    };
}

pub mod migrate {
    use crate::hw::virtio::queue;
    use crate::migrate::*;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct DeviceStateV1 {
        pub status: u8,
        pub queue_select: u16,
        pub negotiated_features: u64,
        pub device_feature_select: u32,
        pub driver_feature_select: u32,
        pub config_generation: u8,
        pub config_generation_seen: bool,
        pub device_config_size: u64,
        pub mode: u32,
        pub msix_cfg_vec: u16,
        pub msix_queue_vec: Vec<u16>,
        pub isr_queue: bool,
        pub isr_cfg: bool,
    }

    #[derive(Deserialize, Serialize)]
    pub struct PciVirtioStateV1 {
        pub device: DeviceStateV1,
        pub queues: queue::migrate::VirtQueuesV1,
    }
    impl Schema<'_> for PciVirtioStateV1 {
        fn id() -> SchemaId {
            ("pci-virtio", 1)
        }
    }
}
