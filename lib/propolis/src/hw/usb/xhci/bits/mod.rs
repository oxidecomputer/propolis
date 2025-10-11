// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Constants and structures for xHCI.

// Not all of these fields may be relevant to us, but they're here for completeness.
#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use crate::{
    common::GuestAddr,
    vmm::{time, VmmHdl},
};
use bitstruct::bitstruct;
use strum::FromRepr;

pub mod device_context;
pub mod ring_data;

/// Statically-known values for Operational/Capability register reads
pub mod values {
    use super::*;
    use crate::hw::usb::xhci::{
        registers::XHC_REGS, MAX_DEVICE_SLOTS, MAX_PORTS, NUM_INTRS,
    };

    pub const HCS_PARAMS1: HcStructuralParameters1 = HcStructuralParameters1(0)
        .with_max_slots(MAX_DEVICE_SLOTS)
        .with_max_intrs(NUM_INTRS)
        .with_max_ports(MAX_PORTS);

    pub const HCS_PARAMS2: HcStructuralParameters2 = HcStructuralParameters2(0)
        .with_ist_as_frame(true)
        .with_iso_sched_threshold(0b111)
        // We don't need any scratchpad buffers
        .with_max_scratchpad_bufs(0)
        .with_scratchpad_restore(false);

    // maximum values for each latency, unlikely as we may be to hit them
    pub const HCS_PARAMS3: HcStructuralParameters3 = HcStructuralParameters3(0)
        .with_u1_dev_exit_latency(10)
        .with_u2_dev_exit_latency(2047);

    lazy_static::lazy_static! {
        pub static ref HCC_PARAMS1: HcCapabilityParameters1 = {
            let extcap_offset = XHC_REGS.extcap_offset();
            assert!(extcap_offset & 3 == 0);
            assert!(extcap_offset / 4 < u16::MAX as usize);
            HcCapabilityParameters1(0).with_ac64(true).with_xecp(
                // xHCI 1.2 table 5-13: offset in 32-bit words from base
                (extcap_offset / 4) as u16,
            )
        };
    }

    pub const HCC_PARAMS2: HcCapabilityParameters2 = HcCapabilityParameters2(0);

    /// Operational register value for reporting supported page sizes.
    /// bit n = 1, if 2^(n+12) is a supported page size.
    /// (we only support 1, that being 4KiB, so this const evals to 1).
    pub const PAGESIZE_XHCI: u32 =
        1 << (crate::common::PAGE_SIZE.trailing_zeros() - 12);
}

/// Size of the USB-specific PCI configuration space.
///
/// See xHCI 1.2 Section 5.2 PCI Configuration Registers (USB)
pub const USB_PCI_CFG_REG_SZ: u8 = 3;

/// Offset of the USB-specific PCI configuration space.
///
/// See xHCI 1.2 Section 5.2 PCI Configuration Registers (USB)
pub const USB_PCI_CFG_OFFSET: u8 = 0x60;

/// Size of the Host Controller Capability Registers (excluding extended capabilities)
pub const XHC_CAP_BASE_REG_SZ: usize = 0x20;

bitstruct! {
    /// Representation of the Frame Length Adjustment Register (FLADJ).
    ///
    /// See xHCI 1.2 Section 5.2.4
    #[derive(Clone, Copy, Debug, Default)]
    pub struct FrameLengthAdjustment(pub u8) {
        /// Frame Length Timing Value (FLADJ)
        ///
        /// Used to select an SOF cycle time by adding 59488 to the value in this field.
        /// Ignored if NFC is set to 1.
        pub fladj: u8 = 0..6;

        /// No Frame Length Timing Capability (NFC)
        ///
        /// If set to 1, the controller does not support a Frame Length Timing Value.
        pub nfc: bool = 6;

        reserved: u8 = 7..8;
    }
}

bitstruct! {
    /// Representation of the Default Best Effort Service Latency \[Deep\] registers (DBESL / DBESLD).
    ///
    /// See xHCI 1.2 Section 5.2.5 & 5.2.6
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DefaultBestEffortServiceLatencies(pub u8) {
        /// Default Best Effort Service Latency (DBESL)
        pub dbesl: u8 = 0..4;

        /// Default Best Effort Service Latency Deep (DBESLD)
        pub dbesld: u8 = 4..8;
    }
}

bitstruct! {
    /// Representation of the Structural Parameters 1 (HCSPARAMS1) register.
    ///
    /// See xHCI 1.2 Section 5.3.3
    #[derive(Clone, Copy, Debug, Default)]
    pub struct HcStructuralParameters1(pub u32) {
        /// Number of Device Slots (MaxSlots)
        ///
        /// Indicates the number of device slots that the host controller supports
        /// (max num of Device Context Structures and Doorbell Array entries).
        ///
        /// Valid values are 1-255, 0 is reserved.
        pub max_slots: u8 = 0..8;

        /// Number of Interrupters (MaxIntrs)
        ///
        /// Indicates the number of interrupters that the host controller supports
        /// (max addressable Interrupter Register Sets).
        /// The value is 1 less than the actual number of interrupters.
        ///
        /// Valid values are 1-1024, 0 is undefined.
        pub max_intrs: u16 = 8..19;

        reserved: u8 = 19..24;

        /// Number of Ports (MaxPorts)
        ///
        /// Indicates the max Port Number value.
        ///
        /// Valid values are 1-255.
        pub max_ports: u8 = 24..32;
    }
}

bitstruct! {
    /// Representation of the Structural Parameters 2 (HCSPARAMS2) register.
    ///
    /// See xHCI 1.2 Section 5.3.4
    #[derive(Clone, Copy, Debug, Default)]
    pub struct HcStructuralParameters2(pub u32) {
        /// Isochronous Scheduling Threshold (IST)
        ///
        /// Minimum distance (in time) required to stay ahead of the controller while adding TRBs.
        pub iso_sched_threshold: u8 = 0..3;

        /// Indicates whether the IST value is in terms of frames (true) or microframes (false).
        pub ist_as_frame: bool = 3;

        /// Event Ring Segment Table Max (ERST Max)
        ///
        /// Max num. of Event Ring Segment Table entries = 2^(ERST Max).
        ///
        /// Valid values are 0-15.
        pub erst_max: u8 = 4..8;

        reserved: u16 = 8..21;

        /// Number of Scratchpad Buffers (Max Scratchpad Bufs Hi)
        ///
        /// High order 5 bits of the number of Scratchpad Buffers that shall be reserved for the
        /// controller.
        max_scratchpad_bufs_hi: u8 = 21..26;

        /// Scratchpad Restore (SPR)
        ///
        /// Whether Scratchpad Buffers should be maintained across power events.
        pub scratchpad_restore: bool = 26;

        /// Number of Scratchpad Buffers (Max Scratchpad Bufs Lo)
        ///
        /// Low order 5 bits of the number of Scratchpad Buffers that shall be reserved for the
        /// controller.
        max_scratchpad_bufs_lo: u8 = 27..32;
    }
}

impl HcStructuralParameters2 {
    #[inline]
    pub const fn max_scratchpad_bufs(&self) -> u16 {
        let lo = self.max_scratchpad_bufs_lo() as u16 | 0b11111;
        let hi = self.max_scratchpad_bufs_hi() as u16 | 0b11111;
        (hi << 5) | lo
    }

    #[inline]
    pub const fn with_max_scratchpad_bufs(self, max: u16) -> Self {
        let lo = max & 0b11111;
        let hi = (max >> 5) & 0b11111;
        self.with_max_scratchpad_bufs_lo(lo as u8)
            .with_max_scratchpad_bufs_hi(hi as u8)
    }
}

bitstruct! {
    /// Representation of the Structural Parameters 3 (HCSPARAMS3) register.
    ///
    /// See xHCI 1.2 Section 5.3.5
    #[derive(Clone, Copy, Debug, Default)]
    pub struct HcStructuralParameters3(pub u32) {
        /// U1 Device Exit Latency
        ///
        /// Worst case latency to transition from U1 to U0.
        ///
        /// Valid values are 0-10 indicating microseconds.
        pub u1_dev_exit_latency: u8 = 0..8;

        reserved: u8 = 8..16;

        /// U2 Device Exit Latency
        ///
        /// Worst case latency to transition from U2 to U0.
        ///
        /// Valid values are 0-2047 indicating microseconds.
        pub u2_dev_exit_latency: u16 = 16..32;
    }
}

bitstruct! {
    /// Representation of the Capability Parameters 1 (HCCPARAMS1) register.
    ///
    /// See xHCI 1.2 Section 5.3.6
    #[derive(Clone, Copy, Debug, Default)]
    pub struct HcCapabilityParameters1(pub u32) {
        /// 64-Bit Addressing Capability (AC64)
        ///
        /// Whether the controller supports 64-bit addressing.
        pub ac64: bool = 0;

        /// BW Negotiation Capability (BNC)
        ///
        /// Whether the controller supports Bandwidth Negotiation.
        pub bnc: bool = 1;

        /// Context Size (CSZ)
        ///
        /// Whether the controller uses the 64-byte Context data structures.
        pub csz: bool = 2;

        /// Port Power Control (PPC)
        ///
        /// Whether the controller supports Port Power Control.
        pub ppc: bool = 3;

        /// Port Indicators (PIND)
        ///
        /// Whether the xHC root hub supports port indicator control.
        pub pind: bool = 4;

        /// Light HC Reset Capability (LHRC)
        ///
        /// Whether the controller supports a Light Host Controller Reset.
        pub lhrc: bool = 5;

        /// Latency Tolerance Messaging Capability (LTC)
        ///
        /// Whether the controller supports Latency Tolerance Messaging.
        pub ltc: bool = 6;

        /// No Secondary SID Support (NSS)
        ///
        /// Whether the controller supports Secondary Stream IDs.
        pub nss: bool = 7;

        /// Parse All Event Data (PAE)
        ///
        /// Whether the controller parses all event data TRBs while advancing to the next TD
        /// after a Short Packet, or it skips all but the first Event Data TRB.
        pub pae: bool = 8;

        /// Stopped - Short Packet Capability (SPC)
        ///
        /// Whether the controller is capable of generating a Stopped - Short Packet
        /// Completion Code.
        pub spc: bool = 9;

        /// Stopped EDTLA Capability (SEC)
        ///
        /// Whether the controller's Stream Context supports a Stopped EDTLA field.
        pub sec: bool = 10;

        /// Contiguous Frame ID Capability (CFC)
        ///
        /// Whether the controller is capable of matching the Frame ID of consecutive
        /// isochronous TDs.
        pub cfc: bool = 11;

        /// Maximum Primary Stream Array Size (MaxPSASize)
        ///
        /// The maximum number of Primary Stream Array entries supported by the controller.
        ///
        /// Primary Stream Array size = 2^(MaxPSASize + 1)
        /// Valid values are 0-15, 0 indicates that Streams are not supported.
        pub max_primary_streams: u8 = 12..16;

        /// xHCI Extended Capabilities Pointer (xECP)
        ///
        /// Offset of the first Extended Capability (in 32-bit words).
        pub xecp: u16 = 16..32;
    }
}

bitstruct! {
    /// Representation of the Capability Parameters 2 (HCCPARAMS2) register.
    ///
    /// See xHCI 1.2 Section 5.3.9
    #[derive(Clone, Copy, Debug, Default)]
    pub struct HcCapabilityParameters2(pub u32) {
        /// U3 Entry Capability (U3C)
        ///
        /// Whether the controller root hub ports support port Suspend Complete
        /// notification.
        pub u3c: bool = 0;

        /// Configure Endpoint Command Max Exit Latency Too Large Capability (CMC)
        ///
        /// Indicates whether a Configure Endpoint Command is capable of generating
        /// a Max Exit Latency Too Large Capability Error.
        pub cmc: bool = 1;

        /// Force Save Context Capability (FSC)
        ///
        /// Whether the controller supports the Force Save Context Capability.
        pub fsc: bool = 2;

        /// Compliance Transition Capability (CTC)
        ///
        /// Inidcates whether the xHC USB3 root hub ports support the Compliance Transition
        /// Enabled (CTE) flag.
        pub ctc: bool = 3;

        /// Large ESIT Payload Capability (LEC)
        ///
        /// Indicates whether the controller supports ESIT Payloads larger than 48K bytes.
        pub lec: bool = 4;

        /// Configuration Information Capability (CIC)
        ///
        /// Indicates whether the controller supports extended Configuration Information.
        pub cic: bool = 5;

        /// Extended TBC Capability (ETC)
        ///
        /// Indicates if the TBC field in an isochronous TRB supports the definition of
        /// Burst Counts greater than 65535 bytes.
        pub etc: bool = 6;

        /// Extended TBC TRB Status Capability (ETC_TSC)
        ///
        /// Indicates if the TBC/TRBSts field in an isochronous TRB has additional
        /// information regarding TRB in the TD.
        pub etc_tsc: bool = 7;

        /// Get/Set Extended Property Capability (GSC)
        ///
        /// Indicates if the controller supports the Get/Set Extended Property commands.
        pub gsc: bool = 8;

        /// Virtualization Based Trusted I/O Capability (VTC)
        ///
        /// Whether the controller supports the Virtualization-based Trusted I/O Capability.
        pub vtc: bool = 9;

        reserved: u32 = 10..32;
    }
}

bitstruct! {
    /// Representation of the USB Command (USBCMD) register.
    ///
    /// See xHCI 1.2 Section 5.4.1
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UsbCommand(pub u32) {
        /// Run/Stop (R/S)
        ///
        /// The controller continues execution as long as this bit is set to 1.
        pub run_stop: bool = 0;

        /// Host Controller Reset (HCRST)
        ///
        /// This control bit is used to reset the controller.
        pub host_controller_reset: bool = 1;

        /// Interrupter Enable (INTE)
        ///
        /// Enables or disables interrupts generated by Interrupters.
        pub interrupter_enable: bool = 2;

        /// Host System Error Enable (HSEE)
        ///
        /// Whether the controller shall assert out-of-band error signaling to the host.
        /// See xHCI 1.2 Section 4.10.2.6
        pub host_system_error_enable: bool = 3;

        reserved: u8 = 4..7;

        /// Light Host Controller Reset (LHCRST)
        ///
        /// This control bit is used to initiate a soft reset of the controller.
        /// (If the LHRC bit in HCCPARAMS is set to 1.)
        pub light_host_controller_reset: bool = 7;

        /// Controller Save State (CSS)
        ///
        /// When set to 1, the controller shall save any internal state.
        /// Always returns 0 when read.
        /// See xHCI 1.2 Section 4.23.2
        pub controller_save_state: bool = 8;

        /// Controller Restore State (CRS)
        ///
        /// When set to 1, the controller shall perform a Restore State operation.
        /// Always returns 0 when read.
        /// See xHCI 1.2 Section 4.23.2
        pub controller_restore_state: bool = 9;

        /// Enable Wrap Event (EWE)
        ///
        /// When set to 1, the controller shall generate an MFINDEX Wrap Event
        /// every time the MFINDEX register transitions from 0x3FFF to 0.
        /// See xHCI 1.2 Section 4.14.2
        pub enable_wrap_event: bool = 10;

        /// Enable U3 MFINDEX Stop (EU3S)
        ///
        /// When set to 1, the controller may stop incrementing MFINDEX if all
        /// Root Hub ports are in the U3, Disconnected, Disabled or Powered-off states.
        /// See xHCI 1.2 Section 4.14.2
        pub enable_u3_mfindex_stop: bool = 11;

        reserved2: u32 = 12;

        /// CEM Enable (CME)
        ///
        /// When set to 1, a Max Exit Latency Too Large Capability Error may be
        /// returned by a Configure Endpoint Command.
        /// See xHCI 1.2 Section 4.23.5.2.2
        pub cem_enable: bool = 13;

        /// Extended TBC Enable (ETE)
        ///
        /// Indicates whether the controller supports Transfer Burst Count (TBC)
        /// values greate than 4 in isochronous TDs.
        /// See xHCI 1.2 Section 4.11.2.3
        pub ete: bool = 14;

        /// Extended TBC TRB Status Enable (TSC_EN)
        ///
        /// Indicates whether the controller supports the ETC_TSC capability.
        /// See xHCI 1.2 Section 4.11.2.3
        pub tsc_enable: bool = 15;

        /// VTIO Enable (VTIOE)
        ///
        /// When set to 1, the controller shall enable the VTIO capability.
        pub vtio_enable: bool = 16;

        reserved3: u32 = 17..32;
    }
}

bitstruct! {
    /// Representation of the USB Status (USBSTS) register.
    ///
    /// See xHCI 1.2 Section 5.4.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UsbStatus(pub u32) {
        /// Host Controller Halted (HCH)
        ///
        /// This bit is set to 0 whenever the R/S bit is set to 1. It is set to 1
        /// when the controller has stopped executing due to the R/S bit being cleared.
        pub host_controller_halted: bool = 0;

        reserved: u8 = 1;

        /// Host System Error (HSE)
        ///
        /// Indicates an error condition preventing continuing normal operation.
        pub host_system_error: bool = 2;

        /// Event Interrupt (EINT)
        ///
        /// The controller sets this bit to 1 when the IP bit of any interrupter
        /// goes from 0 to 1.
        pub event_interrupt: bool = 3;

        /// Port Change Detect (PCD)
        ///
        /// The controller sets this bit to 1 when any port has a change bit flip
        /// from 0 to 1.
        pub port_change_detect: bool = 4;

        reserved2: u8 = 5..8;

        /// Save State Status (SSS)
        ///
        /// A write to the CSS bit in the USBCMD register causes this bit to flip to
        /// 1. The controller shall clear this bit to 0 when the Save State operation
        /// has completed.
        pub save_state_status: bool = 8;

        /// Restore State Status (RSS)
        ///
        /// A write to the CRS bit in the USBCMD register causes this bit to flip to
        /// 1. The controller shall clear this bit to 0 when the Restore State operation
        /// has completed.
        pub restore_state_status: bool = 9;

        /// Save/Restore Error (SRE)
        ///
        /// Indicates that the controller has detected an error condition
        /// during a Save or Restore State operation.
        pub save_restore_error: bool = 10;

        /// Controller Not Ready (CNR)
        ///
        /// Indicates that the controller is not ready to accept doorbell
        /// or runtime register writes.
        pub controller_not_ready: bool = 11;

        /// Host Controller Error (HCE)
        ///
        /// Indicates if the controller has encountered an internal error
        /// that requires a reset to recover.
        pub host_controller_error: bool = 12;

        reserved3: u32 = 13..32;
    }
}

/// Representation of the Device Notification Control (DNCTRL) register.
///
/// Bits: 0-15 Notification Enable (N0-N15)
///
/// When set to 1, the controller shall generate a Device Notification Event
/// when a Device Notification Transaction Packet matching the set bit is received.
///
/// See xHCI 1.2 Sections 5.4.4, 6.4.2.7
pub type DeviceNotificationControl = bitvec::BitArr!(for 16, in u32);

bitstruct! {
    /// Representation of the Command Ring Control (CRCR) register.
    ///
    /// See xHCI 1.2 Section 5.4.5
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CommandRingControl(pub u64) {
        /// Ring Cycle State (RCS)
        ///
        /// Indicates the Consumer Cycle State (CCS) flag for the TRB
        /// referenced by the Command Ring Pointer (CRP).
        pub ring_cycle_state: bool = 0;

        /// Command Stop (CS)
        ///
        /// When set to 1, the controller shall stop the Command Ring operation
        /// after the currently executing command has completed.
        pub command_stop: bool = 1;

        /// Command Abort (CA)
        ///
        /// When set to 1, the controller shall abort the currently executing
        /// command and stop the Command Ring operation.
        pub command_abort: bool = 2;

        /// Command Ring Running (CRR)
        ///
        /// This bit is set to 1 if the R/S bit is 1 and software submitted
        /// a Host Controller Command.
        pub command_ring_running: bool = 3;

        reserved: u8 = 4..6;

        /// Command Ring Pointer (CRP)
        ///
        /// The high order bits of the initial value of the Command Ring Dequeue Pointer.
        command_ring_pointer_: u64 = 6..64;
    }
}

impl CommandRingControl {
    /// The Command Ring Dequeue Pointer.
    #[inline]
    pub fn command_ring_pointer(&self) -> GuestAddr {
        GuestAddr(self.command_ring_pointer_() << 6)
    }

    /// Build register value with the given Command Ring Dequeue Pointer.
    #[inline]
    pub fn with_command_ring_pointer(self, addr: GuestAddr) -> Self {
        self.with_command_ring_pointer_(addr.0 >> 6)
    }

    /// Set the Command Ring Dequeue Pointer.
    #[inline]
    pub fn set_command_ring_pointer(&mut self, addr: GuestAddr) {
        self.set_command_ring_pointer_(addr.0 >> 6)
    }
}

bitstruct! {
    /// Representation of the Configure (CONFIG) register.
    ///
    /// See xHCI 1.2 Section 5.4.7
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Configure(pub u32) {
        /// Max Device Slots Enabled (MaxSlotsEn)
        ///
        /// The maximum number of enabled device slots.
        /// Valid values are 0 to MaxSlots.
        pub max_device_slots_enabled: u8 = 0..8;

        /// U3 Entry Enable (U3E)
        ///
        /// When set to 1, the controller shall assert the PLC flag
        /// when a Root hub port enters U3 state.
        pub u3_entry_enable: bool = 8;

        /// Configuration Information Enable (CIE)
        ///
        /// When set to 1, the software shall initialize the
        /// Configuration Value, Interface Number, and Alternate Setting
        /// fields in the Input Control Context.
        pub configuration_information_enable: bool = 9;

        reserved: u32 = 10..32;
    }
}

/// xHCI 1.2 table 5-27; section 4.19
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum PortLinkState {
    U0 = 0,
    U1 = 1,
    U2 = 2,
    U3Suspended = 3,
    Disabled = 4,
    RxDetect = 5,
    Inactive = 6,
    Polling = 7,
    Recovery = 8,
    HotReset = 9,
    ComplianceMode = 10,
    TestMode = 11,
    Reserved12 = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    Resume = 15,
}

impl From<u8> for PortLinkState {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("PortLinkState should only be converted from a 4-bit field in PortStatusControl")
    }
}
impl Into<u8> for PortLinkState {
    fn into(self) -> u8 {
        self as u8
    }
}

bitstruct! {
    /// Representation of the Port Status and Control (PORTSC) operational register.
    ///
    /// See xHCI 1.2 section 5.4.8
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct PortStatusControl(pub u32) {
        /// Current Connect Status (CCS)
        ///
        /// Read-only to software. True iff a device is connected to the port.
        /// Reflects current state of the port, may not correspond directly
        /// to the event that caused the Connect Status Change bit to be set.
        pub current_connect_status: bool = 0;

        /// Port Enabled/Disabled (PED)
        ///
        /// 1 = Enabled, 0 = Disabled.
        /// Software may *disable* a port by writing a '1' to this register,
        /// but only the xHC may enable a port. Automatically cleared to '0'
        /// by disconnects or faults.
        pub port_enabled_disabled: bool = 1;

        reserved0: bool = 2;

        /// Overcurrent Active (OCA)
        ///
        /// Read-only by software, true iff the port has an over-current condition.
        pub overcurrent_active: bool = 3;

        /// Port Reset (PR)
        ///
        /// When read, true iff port is in reset.
        /// Software may write a '1' to set this register to '1', which
        /// is done to transition a USB2 port from Polling to Enabled.
        pub port_reset: bool = 4;

        /// Port Link State (PLS)
        ///
        /// May only be written when LWS is true.
        pub port_link_state: PortLinkState = 5..9;

        /// Port Power (PP)
        ///
        /// False iff the port is in powered-off state.
        pub port_power: bool = 9;

        /// Port Speed
        ///
        /// Read-only to software.
        /// 0 = Undefined, 1-15 = Protocol Speed ID (PSI).
        /// See xHCI 1.2 section 7.2.1
        pub port_speed: u8 = 10..14;

        /// Port Indicator Control (PIC)
        ///
        /// What color to light the port indicator, if PIND in HCCPARAM1 is set.
        /// (0 = off, 1 = amber, 2 = green, 3 = undefined.) Not used by us.
        pub port_indicator_control: u8 = 14..16;

        /// Port Link State Write Strobe (LWS)
        ///
        /// When true, writes to the Port Link State (PLS) field are enabled.
        pub port_link_state_write_strobe: bool = 16;

        /// Connect Status Change (CSC)
        ///
        /// Indicates a change has occurred in CCS or CAS.
        /// Software clears this bit by writing a '1' to it.
        /// xHC sets to '1' for all changes to the port device connect status,
        /// even if software has not cleared an existing CSC.
        pub connect_status_change: bool = 17;

        /// Port Enabled/Disabled Change (PEC)
        ///
        /// For USB2 ports only, '1' indicates a change in PED.
        /// Software clears flag by writing '1' to it.
        pub port_enabled_disabled_change: bool = 18;

        /// Warm Port Reset Change (WRC)
        ///
        /// For USB3 ports only.
        /// xHC sets to '1' when warm reset processing completes.
        /// Software clears flag by writing '1' to it.
        pub warm_port_reset_change: bool = 19;

        /// Over-current Change (OCC)
        ///
        /// xHC sets to '1' when the value of OCA has changed.
        /// Software clears flag by writing '1' to it.
        pub overcurrent_change: bool = 20;

        /// Port Reset Change (PRC)
        ///
        /// xHC sets to '1' when PR transitions from '1' to '0',
        /// as long as the reset processing was not forced to terminate
        /// due to software clearing PP or PED.
        /// Software clears flag by writing '1' to it.
        pub port_reset_change: bool = 21;

        /// Port Link State Change (PLSC)
        ///
        /// xHC sets to '1' according to conditions described in
        /// sub-table of xHCI 1.2 table 5-27 (bit 22).
        /// Software clears flag by writing '1' to it.
        pub port_link_state_change: bool = 22;

        /// Port Config Error Change (CEC)
        ///
        /// For USB3 ports only, xHC sets to '1' if Port Config Error detected.
        /// Software clears flag by writing '1' to it.
        pub port_config_error_change: bool = 23;

        /// Cold Attach Status (CAS)
        ///
        /// For USB3 only. See xHCI 1.2 sect 4.19.8
        pub cold_attach_status: bool = 24;

        /// Wake on Connect Enable (WCE)
        ///
        /// Software writes '1' to enable sensitivity to device connects
        /// as system wake-up events. See xHCI 1.2 sect 4.15
        pub wake_on_connect_enable: bool = 25;

        /// Wake on Disconnect Enable (WDE)
        ///
        /// Software writes '1' to enable sensitivity to device disconnects
        /// as system wake-up events. See xHCI 1.2 sect 4.15
        pub wake_on_disconnect_enable: bool = 26;

        /// Wake on Overcurrent Enable (WOE)
        ///
        /// Software writes '1' to enable sensitivity to over-current conditions
        /// as system wake-up events. See xHCI 1.2 sect 4.15
        pub wake_on_overcurrent_enable: bool = 27;

        reserved1: u8 = 28..30;

        /// Device Removable (DR) \[sic\]
        ///
        /// True iff the attached device is *non-*removable.
        pub device_nonremovable: bool = 30;

        /// Warm Port Reset (WPR)
        ///
        /// For USB3 only. See xHCI 1.2 sect 4.19.5.1
        pub warm_port_reset: bool = 31;
    }
}

impl PortStatusControl {
    /// xHCI 1.2 sect 4.19.2, figure 4-34: The PSCEG signal that determines
    /// whether an update to PORTSC produces a Port Status Change Event
    pub fn port_status_change_event_generation(&self) -> bool {
        self.connect_status_change()
            || self.port_enabled_disabled_change()
            || self.warm_port_reset_change()
            || self.overcurrent_change()
            || self.port_reset_change()
            || self.port_link_state_change()
            || self.port_config_error_change()
    }
}

/// State of USB2 Link Power Management (LPM). See xHCI 1.2 table 5-30.
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum L1Status {
    /// Ignored by software.
    Invalid = 0,
    /// Port successfully transitioned to L1 (ACK)
    Success = 1,
    /// Device unable to enter L1 at this time (NYET)
    NotYet = 2,
    /// Device does not support L1 transitions (STALL)
    NotSupported = 3,
    /// Device failed to respond to the LPM Transaction or an error occurred
    TimeoutError = 4,
    Reserved5 = 5,
    Reserved6 = 6,
    Reserved7 = 7,
}

impl From<u8> for L1Status {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("L1Status should only be converted from a 3-bit field in PortPowerManagementStatusControlUsb2")
    }
}
impl Into<u8> for L1Status {
    fn into(self) -> u8 {
        self as u8
    }
}

/// See xHCI 1.2 table 5-30; USB 2.0 sects 7.1.20, 11.24.2.13.
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum PortTestControl {
    /// Port is not operating in a test mode.
    TestModeNotEnabled = 0,
    /// Test J_STATE
    JState = 1,
    /// Test K_STATE
    KState = 2,
    /// Test SE0_NAK
    SE0Nak = 3,
    /// Test Packet
    Packet = 4,
    /// Test FORCE_ENABLE
    ForceEnable = 5,
    Reserved6 = 6,
    Reserved7 = 7,
    Reserved8 = 8,
    Reserved9 = 9,
    Reserved10 = 10,
    Reserved11 = 11,
    Reserved12 = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    /// Port Test Control Error
    Error = 15,
}

impl From<u8> for PortTestControl {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("PortTestControl should only be converted from a 4-bit field in PortPowerManagementStatusControlUsb2")
    }
}
impl Into<u8> for PortTestControl {
    fn into(self) -> u8 {
        self as u8
    }
}

bitstruct! {
    /// xHCI 1.2 table 5-30
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortPowerManagementStatusControlUsb2(pub u32) {
        /// L1 Status (L1S)
        ///
        /// Read-only to software. Determines whether an L1 suspend request
        /// (LPM transaction) was successful.
        pub l1_status: L1Status = 0..3;

        /// Remote Wake Enable (RWE)
        ///
        /// Read/write. Software sets this flag to enable/disable the device
        /// for remote wake from L1. While in L1, this flag overrides the
        /// current setting of the Remote Wake feature set by the standard
        /// Set/ClearFeature() commands defined in USB 2.0 chapter 9.
        pub remote_wake_enable: bool = 3;

        /// Best Effort Service Latency (BESL)
        ///
        /// Read/write. Software sets this field to indicate how long the xHC
        /// will drive resume if the xHC initiates an exit from L1.
        /// See xHCI 1.2 section 4.23.5.1.1 and table 4-13.
        pub best_effort_service_latency: u8 = 4..8;

        /// L1 Device Slot
        ///
        /// Read/write. Software sets this to the ID of the Device Slot
        /// associated with the device directly attached to the Root Hub port.
        /// 0 indicates no device is present. xHC uses this to look up info
        /// necessary to generate the LPM token packet.
        pub l1_device_slot: u8 = 8..16;

        /// Hardware LPM Enable (HLE)
        ///
        /// Read/write. If true, enable hardware controlled LPM on this port.
        /// See xHCI 1.2 sect 4.23.5.1.1.1.
        pub hardware_lpm_enable: bool = 16;

        reserved: u16 = 17..28;

        /// Port Test Control (Test Mode)
        ///
        /// Read/write. Indicates whether the port is operating in test mode,
        /// and if so which specific test mode is used.
        pub port_test_control: PortTestControl = 28..32;
    }
}

bitstruct! {
    /// xHCI 1.2 table 5-29
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortPowerManagementStatusControlUsb3(pub u32) {
        pub u1_timeout: u8 = 0..8;
        pub u2_timeout: u8 = 8..16;
        pub force_link_pm_accept: bool = 16;
        reserved: u16 = 17..32;
    }
}

bitstruct! {
    /// xHCI 1.2 table 5-31
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortLinkInfoUsb3(pub u32) {
        /// Link Error Count
        ///
        /// Number of link errors detected by the port, saturating at u16::MAX.
        pub link_error_count: u16 = 0..16;

        /// Rx Lane Count (RLC)
        ///
        /// One less than the number of Receive Lanes negotiated by the port.
        /// Values from 0 to 15 represent Lane Counts of 1 to 16. Read-only.
        pub rx_lane_count: u8 = 16..20;

        /// Tx Lane Count (TLC)
        ///
        /// One less than the number of Transmit Lanes negotiated by the port.
        /// Values from 0 to 15 represent Lane Counts of 1 to 16. Read-only.
        pub tx_lane_count: u8 = 20..24;

        reserved: u8 = 24..32;
    }
}

bitstruct! {
    /// xHCI 1.2 sect 5.4.10.2: The USB2 PORTLI is reserved.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortLinkInfoUsb2(pub u32) {
        reserved: u32 = 0..32;
    }
}

/// xHCI 1.2 table 5-34
#[derive(FromRepr, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum HostInitiatedResumeDurationMode {
    /// Initiate L1 using BESL only on timeout
    BESL = 0,
    /// Initiate L1 using BESLD on timeout. If rejected by device, initiate L1 using BESL.
    BESLD = 1,
    Reserved2 = 2,
    Reserved3 = 3,
}

impl From<u8> for HostInitiatedResumeDurationMode {
    fn from(value: u8) -> Self {
        Self::from_repr(value).expect("HostInitiatedResumeDurationMode should only be converted from a 2-bit field in PortHardwareLpmControlUsb2")
    }
}
impl Into<u8> for HostInitiatedResumeDurationMode {
    fn into(self) -> u8 {
        self as u8
    }
}

bitstruct! {
    /// xHCI 1.2 table 5-34
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortHardwareLpmControlUsb2(pub u32) {
        pub host_initiated_resume_duration_mode: HostInitiatedResumeDurationMode = 0..2;
        pub l1_timeout: u8 = 2..10;
        pub best_effort_service_latency_deep: u8 = 10..14;
        reserved: u32 = 14..32;
    }
}

bitstruct! {
    /// xHCI 1.2 section 5.4.11.3: The USB3 PORTHLPMC register is reserved.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct PortHardwareLpmControlUsb3(pub u32) {
        reserved: u32 = 0..32;
    }
}

bitstruct! {
    /// Representation of the Microframe Index (MFINDEX) runtime register.
    ///
    /// See xHCI 1.2 Section 5.5.1
    #[derive(Clone, Copy, Debug, Default)]
    pub struct MicroframeIndex(pub u32) {
        /// The number of 125-millisecond microframes that have passed,
        /// only incrementing while [UsbCommand::run_stop] has been set to 1.
        /// Read-only.
        pub microframe: u16 = 0..14;

        reserved: u32 = 14..32;
    }
}

/// Minimum Interval Time (MIT) = 125 microseconds. xHCI 1.2 sect 4.14.2
pub const MINIMUM_INTERVAL_TIME: Duration = Duration::from_micros(125);
pub const MFINDEX_WRAP_POINT: u32 = 0x4000;

impl MicroframeIndex {
    /// As MFINDEX is incremented by 1 every 125 microsections while the
    /// controller is running, we compute its value based on the instant
    /// RS was set to 1 in USBCMD.
    pub fn microframe_ongoing(
        &self,
        run_start: &Option<Arc<time::VmGuestInstant>>,
        vmm_hdl: &VmmHdl,
    ) -> u16 {
        let elapsed_microframes = if let Some(then) = run_start {
            let now = time::VmGuestInstant::now(vmm_hdl).unwrap();
            // NOTE: div_duration_f32 not available until 1.80, MSRV is 1.70
            (now.saturating_duration_since(**then).as_secs_f64()
                / MINIMUM_INTERVAL_TIME.as_secs_f64()) as u16
        } else {
            0
        };

        self.microframe().wrapping_add(elapsed_microframes)
    }
}

bitstruct! {
    /// Representation of the Interrupter Management Register (IMAN).
    ///
    /// See xHCI 1.2 Section 5.5.2.1
    #[derive(Clone, Copy, Debug, Default)]
    pub struct InterrupterManagement(pub u32) {
        /// Interrupt Pending (IP)
        ///
        /// True iff an interrupt is pending for this interrupter. RW1C.
        /// See xHCI 1.2 Section 4.17.3 for modification rules.
        pub pending: bool = 0;

        /// Interrupt Enable (IE)
        ///
        /// True if this interrupter is capable of generating an interrupt.
        /// When both this and [Self::pending] are true, the interrupter
        /// shall generate an interrupt when [InterrupterModeration::counter]
        /// reaches 0.
        pub enable: bool = 1;

        reserved: u32 = 2..32;
    }
}

bitstruct! {
    /// Representation of the Interrupter Moderation Register (IMOD).
    ///
    /// See xHCI 1.2 Section 5.5.2.2
    #[derive(Clone, Copy, Debug)]
    pub struct InterrupterModeration(pub u32) {
        /// Interrupt Moderation Interval (IMODI)
        ///
        /// Minimum inter-interrupt interval, specified in 250 nanosecond increments.
        /// 0 disables throttling logic altogether. Default 0x4000 (1 millisecond).
        pub interval: u16 = 0..16;

        /// Interrupt Moderation Counter (IMODC)
        ///
        /// Loaded with the value of [Self::interval] whenever
        /// [InterrupterManagement::pending] is cleared to 0, then counts down
        /// to 0, and then stops. The associated interrupt is signaled when
        /// this counter is 0, the Event Ring is not empty, both flags in
        /// [InterrupterManagement] are true, and
        /// [EventRingDequeuePointer::handler_busy] is false.
        pub counter: u16 = 16..32;
    }
}

pub const IMOD_TICK: Duration = Duration::from_nanos(250);

impl Default for InterrupterModeration {
    fn default() -> Self {
        Self(0).with_interval(0x4000)
    }
}

impl InterrupterModeration {
    pub fn interval_duration(&self) -> std::time::Duration {
        self.interval() as u32 * IMOD_TICK
    }
}

bitstruct! {
    /// Representation of the Event Ring Segment Table Size Register (ERSTSZ)
    ///
    /// See xHCI 1.2 Section 5.5.2.3.1
    // (Note: ERSTSZ, not ersatz -- this is the real deal.)
    #[derive(Clone, Copy, Debug, Default)]
    pub struct EventRingSegmentTableSize(pub u32) {
        /// Number of valid entries in the Event Ring Segment Table pointed to
        /// by [EventRingSegmentTableBaseAddress]. The maximum value is defined
        /// in [HcStructuralParameters2::erst_max].
        ///
        /// For secondary interrupters, writing 0 to this field disables the
        /// Event Ring. Events targeted at this Event Ring while disabled result
        /// in undefined behavior.
        ///
        /// Primary interrupters writing 0 to this field is undefined behavior,
        /// as the Primary Event Ring cannot be disabled.
        pub size: u16 = 0..16;

        reserved: u16 = 16..32;
    }
}

bitstruct! {
    /// Representation of the Event Ring Segment Table Base Address Register (ERSTBA).
    ///
    /// Writing this register starts the Event Ring State Machine.
    /// Secondary interrupters may modify the field at any time.
    /// Primary interrupters  shall not modify this if
    /// [UsbStatus::host_controller_halted] is true.
    ///
    /// See xHCI 1.2 Section 5.5.2.3.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct EventRingSegmentTableBaseAddress(pub u64) {
        reserved: u8 = 0..6;
        /// Default 0. Defines the high-order bits (6..=63) of the start address
        /// of the Event Ring Segment Table.
        address_: u64 = 6..64;
    }
}

impl EventRingSegmentTableBaseAddress {
    pub fn address(&self) -> GuestAddr {
        GuestAddr(self.address_() << 6)
    }
    #[must_use]
    pub const fn with_address(self, value: GuestAddr) -> Self {
        self.with_address_(value.0 >> 6)
    }
    pub fn set_address(&mut self, value: GuestAddr) {
        self.set_address_(value.0 >> 6);
    }
}

bitstruct! {
    /// Representation of the Event Ring Dequeue Pointer Register (ERDP)
    ///
    /// See xHCI 1.2 Section 5.5.2.3.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct EventRingDequeuePointer(pub u64) {
        /// Dequeue ERST Segment Index (DESI). Default 0.
        /// Intended for use by some xHCs to accelerate checking if the
        /// Event Ring is full. (Not used by Propolis at time of writing.)
        pub dequeue_erst_segment_index: u8 = 0..3;

        /// Event Handler Busy (EHB). Default false.
        /// Set to true when Interrupt Pending (IP) is set, cleared by software
        /// (by writing true) when the Dequeue Pointer is written.
        pub handler_busy: bool = 3;

        /// Default 0. Defines the high-order bits (4..=63) of the address of
        /// the current Event Ring Dequeue Pointer.
        pointer_: u64 = 4..64;
    }
}
impl EventRingDequeuePointer {
    pub fn pointer(&self) -> GuestAddr {
        GuestAddr(self.pointer_() << 4)
    }
    #[must_use]
    pub const fn with_pointer(self, value: GuestAddr) -> Self {
        self.with_pointer_(value.0 >> 4)
    }
    pub fn set_pointer(&mut self, value: GuestAddr) {
        self.set_pointer_(value.0 >> 4);
    }
}

bitstruct! {
    /// Representation of a Doorbell Register.
    ///
    /// Software uses this to notify xHC of work to be done for a Device Slot.
    /// From the software's perspective, this should be write-only (reads 0).
    /// See xHCI 1.2 Section 5.6
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DoorbellRegister(pub u32) {
        /// Doorbell Target
        ///
        /// Written value corresponds to a specific xHC notification.
        ///
        /// Values 1..=31 correspond to enqueue pointer updates (see spec).
        /// Values 0 and 32..=247 are reserved.
        /// Values 248..=255 are vendor-defined (and we're the vendor).
        pub db_target: u8 = 0..8;

        reserved: u8 = 8..16;

        /// Doorbell Stream ID
        ///
        /// If the endpoint defines Streams:
        /// - This identifies which the doorbell reference is targeting, and
        /// - 0, 65535 (No Stream), and 65534 (Prime) are reserved values that
        ///   software shall not write to this field.
        ///
        /// If the endpoint does not define Streams, and a nonzero value is
        /// written by software, the doorbell reference is ignored.
        ///
        /// If this is a doorbell is a Host Controller Command Doorbell rather
        /// than a Device Context Doorbell, this field shall be cleared to 0.
        pub db_stream_id: u16 = 16..32;
    }
}

bitstruct! {
    /// Representation of the first 32-bits of the Supported Protocol
    /// Extended Capability field.
    ///
    /// See xHCI 1.2 Section 7.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SupportedProtocol1(pub u32) {
        /// Capability ID. For Supported Protocol, this must be 2
        pub capability_id: u8 = 0..8;

        /// Offset in DWORDs from the start of this register to that of the next.
        /// If there is an Extended Capability after this in the list,
        /// then set to the number of 32-bit DWORDs in the full register.
        /// If this is the last Extended Capability at the XECP, it may be set to 0.
        pub next_capability_pointer: u8 = 8..16;

        /// Minor Revision - binary-coded decimal minor release number of
        /// a USB specification version supported by the xHC.
        pub minor_revision: u8 = 16..24;

        /// Major Revision - binary-coded decimal major release number of
        /// a USB specification version supported by the xHC.
        pub major_revision: u8 = 24..32;
    }
}

/// The second part of the Supported Protocol Extended Capability field
/// See xHCI 1.2 sect 7.2.2 and table 7-7
pub const SUPPORTED_PROTOCOL_2: u32 = u32::from_ne_bytes(*b"USB ");

bitstruct! {
    /// Representation of the third 32-bits of the Supported Protocol
    /// Extended Capability field.
    ///
    /// See xHCI 1.2 Section 7.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SupportedProtocol3(pub u32) {
        /// The starting Port Number of Root Hub ports that support this protocol.
        /// Valid values are 1 to MAX_PORTS.
        pub compatible_port_offset: u8 = 0..8;

        /// The number of consecutive Root Hub ports that support this protocol.
        /// Valid values are 1 to MAX_PORTS.
        pub compatible_port_count: u8 = 8..16;

        /// Protocol-defined definitions.
        /// See xHCI 1.2 section 7.2.2.1.3, tables 7-14 and 7-15
        pub protocol_defined: u16 = 16..28;

        /// Protocol Speed ID Count (PSIC)
        /// Indicates number of Protocol Speed ID (PSI) DWORDs that follow
        /// the SupportedProtocol4 field. May be set to zero, in which case
        /// defaults appropriate for the spec version are used.
        pub protocol_speed_id_count: u8 = 28..32;
    }
}

bitstruct! {
    /// Representation of the fourth 32-bits of the Supported Protocol
    /// Extended Capability field.
    ///
    /// See xHCI 1.2 Section 7.2
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SupportedProtocol4(pub u32) {
        /// Protocol Slot Type - the Slot Type value which may be specified
        /// when allocating Device Slots that support this protocol.
        /// Valid values are ostensibly 0 to 31, but it is also specified that
        /// it "shall be set to 0".
        /// See xHCI 1.2 sect 4.6.3 and 7.2.2.1.4 and table 7-9.
        pub protocol_slot_type: u8 = 0..5;

        reserved: u32 = 5..32;
    }
}
