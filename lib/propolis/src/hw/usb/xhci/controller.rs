// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Emulated USB Host Controller

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::common::{GuestAddr, Lifecycle, RWOp, ReadOp, WriteOp};
use crate::hw::ids::pci::{PROPOLIS_XHCI_DEV_ID, VENDOR_OXIDE};
use crate::hw::pci;
use crate::hw::usb::usbdev::demo_state_tracker::NullUsbDevice;
use crate::hw::usb::xhci::bits::ring_data::TrbCompletionCode;
use crate::hw::usb::xhci::port::PortId;
use crate::hw::usb::xhci::rings::consumer::doorbell;

use super::device_slots::DeviceSlotTable;
use super::rings::consumer::command::CommandRing;
use super::{bits::values::*, registers::*, *};

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_reset() {}
    fn xhci_reg_read(reg_name: &str, value: u64, index: i16) {}
    fn xhci_reg_write(reg_name: &str, value: u64, index: i16) {}
}

pub struct XhciState {
    /// USB Command Register
    usbcmd: bits::UsbCommand,

    /// USB Status Register
    pub(super) usbsts: bits::UsbStatus,

    /// Device Notification Control Register
    dnctrl: bits::DeviceNotificationControl,

    /// Microframe counter (125 ms per tick while running)
    mfindex: bits::MicroframeIndex,

    /// Used for computing MFINDEX while Run/Stop (RS) is set
    run_start: Option<Arc<Instant>>,

    /// If USBCMD EWE is enabled, generates MFINDEX Wrap Events periodically while USBCMD RS=1
    mfindex_wrap_thread: Option<std::thread::JoinHandle<()>>,

    /// Interrupters, including registers and the Event Ring
    pub(super) interrupters: [interrupter::XhciInterrupter; NUM_INTRS as usize],

    pub(super) command_ring: Option<CommandRing>,

    /// Command Ring Control Register (CRCR).
    pub(super) crcr: bits::CommandRingControl,

    pub(super) dev_slots: DeviceSlotTable,

    /// Configure Register
    config: bits::Configure,

    port_regs: [Box<dyn port::XhciUsbPort>; MAX_PORTS as usize + 1],

    /// Event Data Transfer Length Accumulator (EDTLA).
    pub(super) evt_data_xfer_len_accum: u32,

    /// USB devices to attach (currently only supports a proof-of-concept
    /// "device" used for testing basic xHC functionality)
    queued_device_connections: Vec<(PortId, NullUsbDevice)>,
}

impl XhciState {
    fn new(pci_state: &pci::DeviceState, log: slog::Logger) -> Self {
        // The controller is initially halted and asserts CNR (controller not ready)
        let usb_sts = bits::UsbStatus(0)
            .with_host_controller_halted(true)
            .with_controller_not_ready(true);

        let pci_intr = interrupter::XhciPciIntr::new(&pci_state, log.clone());
        let interrupters =
            [interrupter::XhciInterrupter::new(0, pci_intr, log.clone())];

        Self {
            usbcmd: bits::UsbCommand(0),
            usbsts: usb_sts,
            dnctrl: bits::DeviceNotificationControl::new([0]),
            dev_slots: DeviceSlotTable::new(log.clone()),
            config: bits::Configure(0),
            mfindex: bits::MicroframeIndex(0),
            run_start: None,
            mfindex_wrap_thread: None,
            interrupters,
            command_ring: None,
            crcr: bits::CommandRingControl(0),
            port_regs: [
                // dummy value - valid port IDs start at 1
                Box::new(port::NilPort::default()),
                // NUM_USB2_PORTS = 4
                Box::new(port::Usb2Port::default()),
                Box::new(port::Usb2Port::default()),
                Box::new(port::Usb2Port::default()),
                Box::new(port::Usb2Port::default()),
                // NUM_USB3_PORTS = 4
                Box::new(port::Usb3Port::default()),
                Box::new(port::Usb3Port::default()),
                Box::new(port::Usb3Port::default()),
                Box::new(port::Usb3Port::default()),
            ],
            evt_data_xfer_len_accum: 0,
            queued_device_connections: vec![],
        }
    }
}

/// An emulated USB Host Controller attached over PCI
pub struct PciXhci {
    /// PCI device state
    pci_state: pci::DeviceState,

    /// Controller state
    state: Arc<Mutex<XhciState>>,

    log: slog::Logger,
}

impl PciXhci {
    /// Create a new pci-xhci device
    pub fn create(log: slog::Logger) -> Arc<Self> {
        let pci_builder = pci::Builder::new(pci::Ident {
            vendor_id: VENDOR_OXIDE,
            device_id: PROPOLIS_XHCI_DEV_ID,
            sub_vendor_id: VENDOR_OXIDE,
            sub_device_id: PROPOLIS_XHCI_DEV_ID,
            class: pci::bits::CLASS_SERIAL_BUS,
            subclass: pci::bits::SUBCLASS_USB,
            prog_if: pci::bits::PROGIF_USB3,
            ..Default::default()
        });

        let pci_state = pci_builder
            .add_bar_mmio64(pci::BarN::BAR0, 0x2000)
            // Place MSI-X in BAR4
            .add_cap_msix(pci::BarN::BAR4, NUM_INTRS)
            .add_custom_cfg(bits::USB_PCI_CFG_OFFSET, bits::USB_PCI_CFG_REG_SZ)
            .finish();

        let state =
            Arc::new(Mutex::new(XhciState::new(&pci_state, log.clone())));

        Arc::new(Self { pci_state, state, log })
    }

    pub fn add_usb_device(
        &self,
        raw_port: u8,
        // TODO: pass the device when real ones exist
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let port_id = PortId::try_from(raw_port)?;
        let dev = NullUsbDevice::default();
        state.queued_device_connections.push((port_id, dev));
        Ok(())
    }

    /// Handle read of register in USB-specific PCI configuration space
    fn usb_cfg_read(&self, id: UsbPciCfgReg, ro: &mut ReadOp) {
        match id {
            UsbPciCfgReg::SerialBusReleaseNumber => {
                // USB 3.0
                ro.write_u8(0x30);
            }
            UsbPciCfgReg::FrameLengthAdjustment => {
                // We don't support adjusting the SOF cycle
                let fladj = bits::FrameLengthAdjustment(0).with_nfc(true);
                ro.write_u8(fladj.0);
            }
            UsbPciCfgReg::DefaultBestEffortServiceLatencies => {
                // We don't support link power management so return 0
                ro.write_u8(bits::DefaultBestEffortServiceLatencies(0).0);
            }
        }
    }

    /// Handle write to register in USB-specific PCI configuration space
    fn usb_cfg_write(&self, id: UsbPciCfgReg, _wo: &mut WriteOp) {
        match id {
            // Ignore writes to read-only register
            UsbPciCfgReg::SerialBusReleaseNumber => {}

            // We don't support adjusting the SOF cycle
            UsbPciCfgReg::FrameLengthAdjustment => {}

            // We don't support link power management
            UsbPciCfgReg::DefaultBestEffortServiceLatencies => {}
        }
    }

    /// Handle read of memory-mapped host controller register
    fn reg_read(&self, id: Registers, ro: &mut ReadOp) {
        use CapabilityRegisters::*;
        use OperationalRegisters::*;
        use Registers::*;
        use RuntimeRegisters::*;

        use RegRWOpValue::*;
        let mut reg_index = -1;
        let value = match id {
            Reserved => RegRWOpValue::Fill(0),

            // Capability registers
            Cap(CapabilityLength) => {
                // xHCI 1.2 Section 5.3.1: Used to find the beginning of
                // operational registers.
                U8(XHC_REGS.operational_offset() as u8)
            }
            Cap(HciVersion) => {
                // xHCI 1.2 Section 5.3.2: xHCI Version 1.2.0
                U16(0x0120)
            }
            Cap(HcStructuralParameters1) => U32(HCS_PARAMS1.0),
            Cap(HcStructuralParameters2) => U32(HCS_PARAMS2.0),
            Cap(HcStructuralParameters3) => U32(HCS_PARAMS3.0),
            Cap(HcCapabilityParameters1) => U32(HCC_PARAMS1.0),
            Cap(HcCapabilityParameters2) => U32(HCC_PARAMS2.0),
            // Per layout defined in XhcRegMap.
            Cap(DoorbellOffset) => U32(XHC_REGS.doorbell_offset() as u32),
            Cap(RuntimeRegisterSpaceOffset) => {
                U32(XHC_REGS.runtime_offset() as u32)
            }

            // Operational registers
            Op(UsbCommand) => U32(self.state.lock().unwrap().usbcmd.0),
            Op(UsbStatus) => U32(self.state.lock().unwrap().usbsts.0),

            Op(PageSize) => U32(PAGESIZE_XHCI),

            Op(DeviceNotificationControl) => {
                U32(self.state.lock().unwrap().dnctrl.data[0])
            }

            Op(CommandRingControlRegister1) => {
                // xHCI 1.2 table 5-24: Most of these fields read as 0, except for CRR
                let state = self.state.lock().unwrap();
                let crcr = bits::CommandRingControl(0)
                    .with_command_ring_running(
                        state.crcr.command_ring_running(),
                    );
                U32(crcr.0 as u32)
            }
            // xHCI 1.2 table 5-24: The upper region of this register is all
            // upper bits of the command ring pointer, which returns 0 for reads.
            Op(CommandRingControlRegister2) => U32(0),

            Op(DeviceContextBaseAddressArrayPointerRegister) => {
                let state = self.state.lock().unwrap();
                let addr = state.dev_slots.dcbaap().map(|x| x.0).unwrap_or(0);
                U64(addr)
            }
            Op(Configure) => U32(self.state.lock().unwrap().config.0),
            Op(Port(port_id, regs)) => {
                reg_index = port_id.as_raw_id() as i16;
                let state = self.state.lock().unwrap();
                state.port_regs[port_id.as_index()].reg_read(regs)
            }

            // Runtime registers
            Runtime(MicroframeIndex) => {
                let state = self.state.lock().unwrap();
                let mf_adjusted =
                    state.mfindex.microframe_ongoing(&state.run_start);
                U32(state.mfindex.with_microframe(mf_adjusted).0)
            }
            Runtime(Interrupter(i, intr_regs)) => {
                self.state.lock().unwrap().interrupters[i as usize]
                    .reg_read(intr_regs)
            }

            // Only for software to write, returns 0 when read.
            Doorbell(i) => {
                reg_index = i as i16;
                U32(0)
            }

            ExtCap(ExtendedCapabilityRegisters::SupportedProtocol1(i)) => {
                reg_index = i as i16;
                const CAP: bits::SupportedProtocol1 =
                    bits::SupportedProtocol1(0)
                        .with_capability_id(2)
                        .with_next_capability_pointer(4);

                U32(match i {
                    0 => CAP.with_minor_revision(0).with_major_revision(2).0,
                    1 => CAP.with_minor_revision(0).with_major_revision(3).0,
                    // possible values of i defined in registers.rs
                    _ => unreachable!("unsupported SupportedProtocol1 {i}"),
                })
            }
            ExtCap(ExtendedCapabilityRegisters::SupportedProtocol2(i)) => {
                reg_index = i as i16;
                U32(bits::SUPPORTED_PROTOCOL_2)
            }
            ExtCap(ExtendedCapabilityRegisters::SupportedProtocol3(i)) => {
                reg_index = i as i16;
                let cap = bits::SupportedProtocol3::default()
                    .with_protocol_defined(0)
                    .with_protocol_speed_id_count(0);
                U32(match i {
                    0 => {
                        cap.with_compatible_port_offset(1)
                            .with_compatible_port_count(NUM_USB2_PORTS)
                            .0
                    }
                    1 => {
                        cap.with_compatible_port_offset(1 + NUM_USB2_PORTS)
                            .with_compatible_port_count(NUM_USB3_PORTS)
                            .0
                    }
                    // possible values of i defined in registers.rs
                    _ => unreachable!("unsupported SupportedProtocol3 {i}"),
                })
            }
            ExtCap(ExtendedCapabilityRegisters::SupportedProtocol4(i)) => {
                reg_index = i as i16;
                U32(bits::SupportedProtocol4::default()
                    .with_protocol_slot_type(0)
                    .0)
            }
            // end of list of ext caps
            ExtCap(ExtendedCapabilityRegisters::Reserved) => U32(0),
        };

        match value {
            RegRWOpValue::NoOp => {}
            RegRWOpValue::U8(x) => ro.write_u8(x),
            RegRWOpValue::U16(x) => ro.write_u16(x),
            RegRWOpValue::U32(x) => ro.write_u32(x),
            RegRWOpValue::U64(x) => ro.write_u64(x),
            RegRWOpValue::Fill(x) => ro.fill(x),
        }

        let reg_name = id.reg_name();
        let reg_value = value.as_u64();
        probes::xhci_reg_read!(|| (reg_name, reg_value, reg_index));
    }

    /// Handle write to memory-mapped host controller register
    fn reg_write(&self, id: Registers, wo: &mut WriteOp) {
        use OperationalRegisters::*;
        use RegRWOpValue::*;
        use Registers::*;
        use RuntimeRegisters::*;

        let mut reg_index = -1;
        let written_value = match id {
            // Ignore writes to reserved bits
            Reserved => NoOp,

            // Capability registers are all read-only; ignore any writes
            Cap(_) => NoOp,

            // Operational registers
            Op(UsbCommand) => {
                let mut state = self.state.lock().unwrap();
                let cmd = bits::UsbCommand(wo.read_u32());

                // xHCI 1.2 Section 5.4.1.1
                if cmd.run_stop() && !state.usbcmd.run_stop() {
                    if !state.usbsts.host_controller_halted() {
                        slog::error!(
                            self.log,
                            "USBCMD Run while not Halted: undefined behavior!"
                        );
                    }
                    state.usbsts.set_host_controller_halted(false);

                    // xHCI 1.2 sect 4.3
                    let mut queued_conns = Vec::new();
                    core::mem::swap(
                        &mut queued_conns,
                        &mut state.queued_device_connections,
                    );
                    for (port_id, usb_dev) in queued_conns {
                        let memctx = self.pci_state.acc_mem.access().unwrap();
                        if let Some(evt) = state.port_regs[port_id.as_index()]
                            .xhc_update_portsc(
                                &|portsc| {
                                    *portsc = portsc
                                        .with_current_connect_status(true)
                                        .with_port_enabled_disabled(false)
                                        .with_port_reset(false)
                                        .with_port_link_state(
                                            bits::PortLinkState::Polling,
                                        );
                                },
                                port_id,
                            )
                        {
                            state.usbsts.set_event_interrupt(true);
                            if let Err(e) = state.interrupters[0]
                                .enqueue_event(evt, &memctx, false)
                            {
                                slog::error!(&self.log, "unable to signal Port Status Change for device attach: {e}");
                            }
                        }
                        state.usbsts.set_port_change_detect(true);
                        if let Err(_) = state
                            .dev_slots
                            .attach_to_root_hub_port_address(port_id, usb_dev)
                        {
                            slog::error!(&self.log, "root hub port {port_id:?} already had a device attached");
                        }
                    }

                    slog::debug!(
                        self.log,
                        "command ring at {:#x}",
                        state.crcr.command_ring_pointer().0
                    );
                    // unwrap: crcr.command_ring_pointer() can only return 64-aligned values
                    state.command_ring = Some(
                        CommandRing::new(
                            state.crcr.command_ring_pointer(),
                            state.crcr.ring_cycle_state(),
                        )
                        .unwrap(),
                    );

                    // for MFINDEX computation
                    state.run_start = Some(Arc::new(Instant::now()));
                    if state.usbcmd.enable_wrap_event() {
                        self.start_mfindex_wrap_thread(&mut state);
                    }
                } else if !cmd.run_stop() && state.usbcmd.run_stop() {
                    // stop running/queued commands and transfers on all device slots.

                    // apply new MFINDEX value based on time elapsed running
                    let run_start = state.run_start.take();
                    let mf_index = state.mfindex.microframe_ongoing(&run_start);
                    state.mfindex.set_microframe(mf_index);
                    drop(run_start); // makes wrap-event thread stop by losing its Weak ref

                    state.usbsts.set_host_controller_halted(true);
                    // xHCI 1.2 table 5-24: cleared to 0 when R/S is.
                    state.crcr.set_command_ring_running(false);
                }

                // xHCI 1.2 table 5-20: Any transactions in progress are
                // immediately terminated; all internal pipelines, registers,
                // timers, counters, state machines, etc. are reset to their
                // initial value.
                if cmd.host_controller_reset() {
                    let mut devices = Vec::new();
                    core::mem::swap(
                        &mut devices,
                        &mut state.queued_device_connections,
                    );
                    devices.extend(state.dev_slots.detach_all_for_reset());

                    *state = XhciState::new(&self.pci_state, self.log.clone());
                    state.queued_device_connections = devices;

                    state.usbsts.set_controller_not_ready(false);
                    slog::debug!(self.log, "xHC reset");
                    probes::xhci_reset!(|| ());
                    return;
                }

                let usbcmd_inte = cmd.interrupter_enable();
                if usbcmd_inte != state.usbcmd.interrupter_enable() {
                    for interrupter in &mut state.interrupters {
                        interrupter.set_usbcmd_inte(usbcmd_inte);
                    }
                    slog::debug!(
                        self.log,
                        "Interrupter Enabled: {usbcmd_inte}",
                    );
                }

                // xHCI 1.2 Section 4.10.2.6
                if cmd.host_system_error_enable() {
                    slog::debug!(
                        self.log,
                        "USBCMD HSEE unused (USBSTS HSE unimplemented)"
                    );
                }

                // xHCI 1.2 Section 4.23.2.1
                if cmd.controller_save_state() {
                    if state.usbsts.save_state_status() {
                        slog::error!(
                            self.log,
                            "save state while saving: undefined behavior!"
                        );
                    }
                    if state.usbsts.host_controller_halted() {
                        slog::error!(
                            self.log,
                            "unimplemented USBCMD: Save State"
                        );
                    }
                }
                // xHCI 1.2 Section 4.23.2
                if cmd.controller_restore_state() {
                    if state.usbsts.save_state_status() {
                        slog::error!(
                            self.log,
                            "restore state while saving: undefined behavior!"
                        );
                    }
                    if state.usbsts.host_controller_halted() {
                        slog::error!(
                            self.log,
                            "unimplemented USBCMD: Restore State"
                        );
                    }
                }

                // xHCI 1.2 Section 4.14.2
                if cmd.enable_wrap_event() && !state.usbcmd.enable_wrap_event()
                {
                    self.start_mfindex_wrap_thread(&mut state);
                } else if !cmd.enable_wrap_event()
                    && state.usbcmd.enable_wrap_event()
                {
                    self.stop_mfindex_wrap_thread(&mut state);
                }

                // xHCI 1.2 Section 4.14.2
                if cmd.enable_u3_mfindex_stop() {
                    slog::error!(
                        self.log,
                        "unimplemented USBCMD: Enable U3 MFINDEX Stop"
                    );
                }

                // xHCI 1.2 Section 4.23.5.2.2
                if cmd.cem_enable() {
                    slog::error!(self.log, "unimplemented USBCMD: CEM Enable");
                }

                // xHCI 1.2 Section 4.11.2.3
                if cmd.ete() {
                    slog::error!(
                        self.log,
                        "unimplemented USBCMD: ETE (Extended TBC Enable)"
                    );
                }

                // xHCI 1.2 Section 4.11.2.3
                if cmd.tsc_enable() {
                    slog::error!(
                        self.log,
                        "unimplemented USBCMD: Extended TSC TRB Status Enable"
                    );
                }

                // LHCRST is optional, and when it is not implemented
                // (HCCPARAMS1), it must always return 0 when read.
                // CSS and CRS also must always return 0 when read.
                state.usbcmd = cmd
                    .with_host_controller_reset(false)
                    .with_controller_save_state(false)
                    .with_controller_restore_state(false)
                    .with_light_host_controller_reset(false);

                U32(cmd.0)
            }
            // xHCI 1.2 Section 5.4.2
            Op(UsbStatus) => {
                let mut state = self.state.lock().unwrap();
                // HCH, SSS, RSS, CNR, and HCE are read-only (ignored here).
                // HSE, EINT, PCD, and SRE are RW1C (guest writes a 1 to
                // clear a field to 0, e.g. to ack an interrupt we gave it).
                let sts = bits::UsbStatus(wo.read_u32());
                if sts.host_system_error() {
                    state.usbsts.set_host_system_error(false);
                }
                if sts.event_interrupt() {
                    state.usbsts.set_event_interrupt(false);
                }
                if sts.port_change_detect() {
                    state.usbsts.set_port_change_detect(false);
                }
                if sts.save_restore_error() {
                    state.usbsts.set_save_restore_error(false);
                }
                U32(sts.0)
            }
            // Page size is read-only.
            Op(PageSize) => RegRWOpValue::NoOp,
            // xHCI 1.2 sections 5.4.4, 6.4.2.7.
            // Bitfield enabling/disabling Device Notification Events
            // when Device Notification Transaction Packets are received
            // for each of 16 possible notification types
            Op(DeviceNotificationControl) => {
                let mut state = self.state.lock().unwrap();
                let val = wo.read_u32();
                state.dnctrl.data[0] = val & 0xFFFFu32;
                U32(val)
            }
            Op(CommandRingControlRegister1) => {
                let crcr = bits::CommandRingControl(wo.read_u32() as u64);
                let mut state = self.state.lock().unwrap();
                // xHCI 1.2 sections 4.9.3, 5.4.5
                if state.crcr.command_ring_running() {
                    // xHCI 1.2 table 5-24
                    if crcr.command_stop() {
                        // wait for command ring idle, generate command completion event
                        let memctx = self.pci_state.acc_mem.access().unwrap();
                        doorbell::command_ring_stop(
                            &mut state,
                            TrbCompletionCode::CommandRingStopped,
                            &memctx,
                            &self.log,
                        );
                    } else if crcr.command_abort() {
                        // XXX: this doesn't actually abort ongoing processing
                        let memctx = self.pci_state.acc_mem.access().unwrap();
                        doorbell::command_ring_stop(
                            &mut state,
                            TrbCompletionCode::CommandAborted,
                            &memctx,
                            &self.log,
                        );
                    } else {
                        slog::error!(
                            self.log,
                            "wrote CRCR while running: {crcr:?}"
                        );
                    }
                } else {
                    state.crcr = crcr;
                }
                U32(crcr.0 as u32)
            }
            // xHCI 5.1 - 64-bit registers can be written as {lower dword, upper dword},
            // and in CRCR's case this matters, because read-modify-write for each half
            // doesn't work when reads are defined to return 0.
            Op(CommandRingControlRegister2) => {
                let mut state = self.state.lock().unwrap();
                let val = wo.read_u32();
                // xHCI 1.2 sections 4.9.3, 5.4.5
                if !state.crcr.command_ring_running() {
                    state.crcr.0 &= 0xFFFFFFFFu64;
                    state.crcr.0 |= (val as u64) << 32;
                }
                U32(val)
            }
            Op(DeviceContextBaseAddressArrayPointerRegister) => {
                let mut state = self.state.lock().unwrap();
                let dcbaap = GuestAddr(wo.read_u64());
                state.dev_slots.set_dcbaap(dcbaap);
                U64(dcbaap.0)
            }
            Op(Configure) => {
                let mut state = self.state.lock().unwrap();
                state.config = bits::Configure(wo.read_u32());
                U32(state.config.0)
            }
            Op(Port(port_id, regs)) => {
                reg_index = port_id.as_raw_id() as i16;
                let mut state = self.state.lock().unwrap();
                let port = &mut state.port_regs[port_id.as_index()];
                // all implemented port regs are 32-bit
                let value = wo.read_u32();
                match port.reg_write(value, regs, &self.log) {
                    // xHCI 1.2 sect 4.19.5
                    port::PortWrite::BusReset => {
                        // NOTE: do USB bus reset seq with device in port here.
                        // USB2 ports are specified as being unable
                        // to fail the bus reset sequence.

                        let memctx = self.pci_state.acc_mem.access().unwrap();
                        if let Some(evt) = port.xhc_update_portsc(
                            &|portsc| {
                                *portsc = portsc
                                    .with_port_link_state(
                                        bits::PortLinkState::U0,
                                    )
                                    .with_port_reset(false)
                                    .with_port_enabled_disabled(true)
                                    .with_port_reset_change(true)
                                    .with_port_speed(0);
                            },
                            port_id,
                        ) {
                            state.usbsts.set_event_interrupt(true);
                            if let Err(e) = state.interrupters[0]
                                .enqueue_event(evt, &memctx, false)
                            {
                                slog::error!(&self.log, "unable to signal Port Status Change for bus reset: {e}");
                            }
                        }
                    }
                    _ => {}
                }
                U32(value)
            }

            // Runtime registers
            Runtime(MicroframeIndex) => NoOp, // Read-only
            Runtime(Interrupter(i, intr_regs)) => {
                reg_index = i as i16;
                let mut state = self.state.lock().unwrap();
                let memctx = self.pci_state.acc_mem.access().unwrap();
                state.interrupters[i as usize].reg_write(wo, intr_regs, &memctx)
            }

            Doorbell(0) => {
                reg_index = 0;
                slog::debug!(self.log, "doorbell 0");
                // xHCI 1.2 section 4.9.3, table 5-43
                let doorbell_register = bits::DoorbellRegister(wo.read_u32());
                if doorbell_register.db_target() == 0 {
                    let mut state = self.state.lock().unwrap();
                    // xHCI 1.2 table 5-24: only set to 1 if R/S is 1
                    if state.usbcmd.run_stop() {
                        state.crcr.set_command_ring_running(true);
                    }
                    let memctx = self.pci_state.acc_mem.access().unwrap();
                    doorbell::process_command_ring(
                        &mut state, &memctx, &self.log,
                    );
                }
                U32(doorbell_register.0)
            }
            // xHCI 1.2 section 4.7
            Doorbell(slot_id) => {
                reg_index = slot_id as i16;
                // TODO: care about DoorbellRegister::db_stream_id for USB3
                let doorbell_register = bits::DoorbellRegister(wo.read_u32());
                let endpoint_id = doorbell_register.db_target();
                slog::debug!(
                    self.log,
                    "doorbell slot {slot_id} ep {endpoint_id}"
                );
                let mut state = self.state.lock().unwrap();
                let memctx = self.pci_state.acc_mem.access().unwrap();
                doorbell::process_transfer_ring(
                    &mut state,
                    slot_id,
                    endpoint_id,
                    &memctx,
                    &self.log,
                );
                U32(doorbell_register.0)
            }

            // read-only
            ExtCap(_) => NoOp,
        };

        let reg_value = written_value.as_u64();
        let reg_name = id.reg_name();
        probes::xhci_reg_write!(|| (reg_name, reg_value, reg_index));
    }

    // xHCI 1.2 sect 4.14.2 - generate a MFINDEX Wrap Event every time
    // MFINDEX's microframe value wraps from 0x3FFF to 0
    fn start_mfindex_wrap_thread(&self, state: &mut XhciState) {
        if let Some(run_start_arc) = state.run_start.as_ref() {
            let initial_value = state.mfindex.microframe() as u32;
            let first_wrap_time = run_start_arc
                .checked_add(
                    (bits::MFINDEX_WRAP_POINT - initial_value)
                        * bits::MINIMUM_INTERVAL_TIME,
                )
                .unwrap_or_else(|| {
                    slog::error!(
                        self.log,
                        "unrepresentable instant in mfindex wrap"
                    );
                    // fudge the numbers a bit and maintain periodicity
                    Instant::now()
                });
            let run_start_weak = Arc::downgrade(run_start_arc);
            let state_weak = Arc::downgrade(&self.state);
            let acc_mem = self
                .pci_state
                .acc_mem
                .child(Some("MFINDEX Wrap Event thread".to_string()));

            state.mfindex_wrap_thread = Some(std::thread::spawn(move || {
                use rings::producer::event::EventInfo;
                let mut wraps = 0;
                while run_start_weak.upgrade().is_some() {
                    // sleep_until https://github.com/rust-lang/rust/issues/113752
                    let deadline = first_wrap_time
                        + bits::MINIMUM_INTERVAL_TIME
                            * bits::MFINDEX_WRAP_POINT
                            * wraps;
                    wraps += 1;
                    if let Some(delay) =
                        deadline.checked_duration_since(Instant::now())
                    {
                        std::thread::sleep(delay);
                    }
                    // enqueue event
                    let Some(state_arc) = state_weak.upgrade() else {
                        break;
                    };
                    let Ok(mut state) = state_arc.lock() else {
                        break;
                    };
                    let memctx = acc_mem.access().unwrap();
                    state.usbsts.set_event_interrupt(true);
                    state.interrupters[0]
                        .enqueue_event(EventInfo::MfIndexWrap, &memctx, false)
                        .ok(); // shall be dropped by the xHC if Event Ring full
                }
            }));
        }
    }

    fn stop_mfindex_wrap_thread(&self, state: &mut XhciState) {
        if let Some(run_start) = state.run_start.take() {
            // replace Arc so thread's Weak ref dies
            state.run_start = Some(Arc::new(*run_start));
        }
        if let Some(jh) = state.mfindex_wrap_thread.take() {
            if let Err(_) = jh.join() {
                slog::error!(
                    self.log,
                    "mfindex wrap event thread failed to join"
                );
            }
        }
    }
}

impl Lifecycle for PciXhci {
    fn type_name(&self) -> &'static str {
        "pci-xhci"
    }
}

impl pci::Device for PciXhci {
    fn device_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }

    fn cfg_rw(&self, region: u8, mut rwo: RWOp) {
        assert_eq!(region, bits::USB_PCI_CFG_OFFSET);

        USB_PCI_CFG_REGS.process(
            &mut rwo,
            |id: &UsbPciCfgReg, rwo: RWOp<'_, '_>| match rwo {
                RWOp::Read(ro) => self.usb_cfg_read(*id, ro),
                RWOp::Write(wo) => self.usb_cfg_write(*id, wo),
            },
        )
    }

    fn bar_rw(&self, bar: pci::BarN, mut rwo: RWOp) {
        assert_eq!(bar, pci::BarN::BAR0);
        XHC_REGS.map.process(&mut rwo, |id: &Registers, rwo: RWOp<'_, '_>| {
            match rwo {
                RWOp::Read(ro) => self.reg_read(*id, ro),
                RWOp::Write(wo) => self.reg_write(*id, wo),
            }
        })
    }

    fn interrupt_mode_change(&self, mode: pci::IntrMode) {
        let mut state = self.state.lock().unwrap();
        for interrupter in &mut state.interrupters {
            interrupter.set_pci_intr_mode(mode);
        }
    }
}
