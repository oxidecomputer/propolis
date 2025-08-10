// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XHCI Registers

#![allow(dead_code)]

use crate::util::regmap::RegMap;

use super::bits;
use super::port::PortId;
use super::{MAX_DEVICE_SLOTS, MAX_PORTS, NUM_INTRS};

use lazy_static::lazy_static;

/// USB-specific PCI configuration registers.
///
/// See xHCI 1.2 Section 5.2 PCI Configuration Registers (USB)
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UsbPciCfgReg {
    /// Serial Bus Release Number Register (SBRN)
    ///
    /// Indicates which version of the USB spec the controller implements.
    ///
    /// See xHCI 1.2 Section 5.2.3
    SerialBusReleaseNumber,

    /// Frame Length Adjustment Register (FLADJ)
    ///
    /// See xHCI 1.2 Section 5.2.4
    FrameLengthAdjustment,

    /// Default Best Effort Service Latency \[Deep\] (DBESL / DBESLD)
    ///
    /// See xHCI 1.2 Section 5.2.5 & 5.2.6
    DefaultBestEffortServiceLatencies,
}

lazy_static! {
    pub static ref USB_PCI_CFG_REGS: RegMap<UsbPciCfgReg> = {
        use UsbPciCfgReg::*;

        let layout = [
            (SerialBusReleaseNumber, 1),
            (FrameLengthAdjustment, 1),
            (DefaultBestEffortServiceLatencies, 1),
        ];

        RegMap::create_packed(bits::USB_PCI_CFG_REG_SZ.into(), &layout, None)
    };
}

/// Registers in MMIO space pointed to by BAR0/1
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Registers {
    Reserved,
    Cap(CapabilityRegisters),
    Op(OperationalRegisters),
    Runtime(RuntimeRegisters),
    Doorbell(u8),
    ExtCap(ExtendedCapabilityRegisters),
}

/// eXtensible Host Controller Capability Registers
///
/// See xHCI 1.2 Section 5.3 Host Controller Capability Registers
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CapabilityRegisters {
    CapabilityLength,
    HciVersion,
    HcStructuralParameters1,
    HcStructuralParameters2,
    HcStructuralParameters3,
    HcCapabilityParameters1,
    HcCapabilityParameters2,
    DoorbellOffset,
    RuntimeRegisterSpaceOffset,
}

/// eXtensible Host Controller Operational Port Registers
///
/// See xHCI 1.2 Sections 5.4.8-5.4.11
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PortRegisters {
    PortStatusControl,
    PortPowerManagementStatusControl,
    PortLinkInfo,
    PortHardwareLpmControl,
}

/// eXtensible Host Controller Operational Registers
///
/// See xHCI 1.2 Section 5.4 Host Controller Operational Registers
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OperationalRegisters {
    UsbCommand,
    UsbStatus,
    PageSize,
    DeviceNotificationControl,
    CommandRingControlRegister1,
    CommandRingControlRegister2,
    DeviceContextBaseAddressArrayPointerRegister,
    Configure,
    Port(PortId, PortRegisters),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InterrupterRegisters {
    Management,
    Moderation,
    EventRingSegmentTableSize,
    EventRingSegmentTableBaseAddress,
    EventRingDequeuePointer,
}

/// eXtensible Host Controller Runtime Registers
///
/// See xHCI 1.2 Section 5.5 Host Controller Runtime Registers
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RuntimeRegisters {
    MicroframeIndex,
    Interrupter(u16, InterrupterRegisters),
}

/// eXtensible Host Controller Capability Registers
///
/// See xHCI 1.2 Section 7 xHCI Extended Capabilities
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExtendedCapabilityRegisters {
    SupportedProtocol1(u8),
    SupportedProtocol2(u8),
    SupportedProtocol3(u8),
    SupportedProtocol4(u8),
    // used for end of list
    Reserved,
}

pub struct XhcRegMap {
    pub map: RegMap<Registers>,
    pub cap_len: usize,
    pub op_len: usize,
    pub run_len: usize,
    pub db_len: usize,
}

impl XhcRegMap {
    pub(super) const fn operational_offset(&self) -> usize {
        self.cap_len
    }
    pub(super) const fn runtime_offset(&self) -> usize {
        self.operational_offset() + self.op_len
    }
    pub(super) const fn doorbell_offset(&self) -> usize {
        self.runtime_offset() + self.run_len
    }
    pub(super) const fn extcap_offset(&self) -> usize {
        self.doorbell_offset() + self.db_len
    }
}

lazy_static! {
    pub static ref XHC_REGS: XhcRegMap = {
        use CapabilityRegisters::*;
        use OperationalRegisters::*;
        use RuntimeRegisters::*;
        use Registers::*;

        // xHCI 1.2 Table 5-9
        // (may be expanded if implementing extended capabilities)
        let cap_layout = [
            (Cap(CapabilityLength), 1),
            (Reserved, 1),
            (Cap(HciVersion), 2),
            (Cap(HcStructuralParameters1), 4),
            (Cap(HcStructuralParameters2), 4),
            (Cap(HcStructuralParameters3), 4),
            (Cap(HcCapabilityParameters1), 4),
            (Cap(DoorbellOffset), 4),
            (Cap(RuntimeRegisterSpaceOffset), 4),
            (Cap(HcCapabilityParameters2), 4),
        ].into_iter();

        let op_layout = [
            (Op(UsbCommand), 4),
            (Op(UsbStatus), 4),
            (Op(PageSize), 4),
            (Reserved, 8),
            (Op(DeviceNotificationControl), 4),
            (Op(CommandRingControlRegister1), 4),
            (Op(CommandRingControlRegister2), 4),
            (Reserved, 16),
            (Op(DeviceContextBaseAddressArrayPointerRegister), 8),
            (Op(Configure), 4),
            (Reserved, 964),
        ].into_iter();

        // Add the port registers
        let op_layout = op_layout.chain((1..=MAX_PORTS).flat_map(|i| {
            use PortRegisters::*;
            let port_id = PortId::try_from(i).unwrap();
            [
                (Op(OperationalRegisters::Port(port_id, PortStatusControl)), 4),
                (Op(OperationalRegisters::Port(port_id, PortPowerManagementStatusControl)), 4),
                (Op(OperationalRegisters::Port(port_id, PortLinkInfo)), 4),
                (Op(OperationalRegisters::Port(port_id, PortHardwareLpmControl)), 4),
            ]
        }));

        let run_layout = [
            (Runtime(MicroframeIndex), 4),
            (Reserved, 28),
        ].into_iter();
        let run_layout = run_layout.chain((0..NUM_INTRS).flat_map(|i| {
            use InterrupterRegisters::*;
            [
                (Runtime(Interrupter(i, Management)), 4),
                (Runtime(Interrupter(i, Moderation)), 4),
                (Runtime(Interrupter(i, EventRingSegmentTableSize)), 4),
                (Reserved, 4),
                (Runtime(Interrupter(i, EventRingSegmentTableBaseAddress)), 8),
                (Runtime(Interrupter(i, EventRingDequeuePointer)), 8),
            ]
        }));

        // +1: 0th doorbell is Command Ring's.
        let db_layout = (0..MAX_DEVICE_SLOTS + 1).map(|i| (Doorbell(i), 4));

        let extcap_layout = [
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol1(0)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol2(0)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol3(0)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol4(0)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol1(1)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol2(1)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol3(1)), 4),
            (ExtCap(ExtendedCapabilityRegisters::SupportedProtocol4(1)), 4),
            (ExtCap(ExtendedCapabilityRegisters::Reserved), 4),
        ].into_iter();

        // Stash the lengths for later use.
        let cap_len = cap_layout.clone().map(|(_, sz)| sz).sum();
        let op_len = op_layout.clone().map(|(_, sz)| sz).sum();
        let run_len = run_layout.clone().map(|(_, sz)| sz).sum();
        let db_len = db_layout.clone().map(|(_, sz)| sz).sum();
        let extcap_len: usize = extcap_layout.clone().map(|(_, sz)| sz).sum();

        let layout = cap_layout
            .chain(op_layout)
            .chain(run_layout)
            .chain(db_layout)
            .chain(extcap_layout);

        assert_eq!(cap_len, bits::XHC_CAP_BASE_REG_SZ);

        let xhc_reg_map = XhcRegMap {
            map: RegMap::create_packed_iter(
                cap_len + op_len + run_len + db_len + extcap_len,
                layout,
                Some(Reserved),
            ),
            cap_len,
            op_len,
            run_len,
            db_len,
        };

        // xHCI 1.2 Table 5-2:
        // Capability registers must be page-aligned, and they're first.
        // Operational-registers must be 4-byte-aligned. They follow cap regs.
        // `cap_len` is a multiple of 4 (32 at time of writing).
        assert_eq!(xhc_reg_map.operational_offset() % 4, 0);
        // Runtime registers must be 32-byte-aligned.
        // Both `cap_len` and `op_len` are (at present, cap_len is 1024 + 16*8),
        // so we can safely put Runtime registers immediately after them.
        // (Note: if VTIO is implemented, virtual fn's must be *page*-aligned)
        assert_eq!(xhc_reg_map.runtime_offset() % 32, 0);
        // Finally, the Doorbell array merely must be 4-byte-aligned.
        // All the runtime registers immediately preceding are 4 bytes wide.
        assert_eq!(xhc_reg_map.doorbell_offset() % 4, 0);

        xhc_reg_map
    };
}

impl Registers {
    /// Returns the abbreviation (or name, where unabbreviated) of the register
    /// from the xHCI 1.2 specification
    pub const fn reg_name(&self) -> &'static str {
        use Registers::*;
        match self {
            Reserved => "RsvdZ.",
            Cap(capability_registers) => {
                use CapabilityRegisters::*;
                match capability_registers {
                    CapabilityLength => "CAPLENGTH",
                    HciVersion => "HCIVERSION",
                    HcStructuralParameters1 => "HCSPARAMS1",
                    HcStructuralParameters2 => "HCSPARAMS2",
                    HcStructuralParameters3 => "HCSPARAMS3",
                    HcCapabilityParameters1 => "HCCPARAMS1",
                    HcCapabilityParameters2 => "HCCPARAMS2",
                    DoorbellOffset => "DBOFF",
                    RuntimeRegisterSpaceOffset => "RTSOFF",
                }
            }
            Op(operational_registers) => {
                use OperationalRegisters::*;
                match operational_registers {
                    UsbCommand => "USBCMD",
                    UsbStatus => "USBSTS",
                    PageSize => "PAGESIZE",
                    DeviceNotificationControl => "DNCTRL",
                    CommandRingControlRegister1 => "CRCR",
                    CommandRingControlRegister2 => {
                        "(upper DWORD of 64-bit CRCR)"
                    }
                    DeviceContextBaseAddressArrayPointerRegister => "DCBAAP",
                    Configure => "CONFIG",
                    Port(_, port_registers) => {
                        use PortRegisters::*;
                        match port_registers {
                            PortStatusControl => "PORTSC",
                            PortPowerManagementStatusControl => "PORTPMSC",
                            PortLinkInfo => "PORTLI",
                            PortHardwareLpmControl => "PORTHLPMC",
                        }
                    }
                }
            }
            Runtime(runtime_registers) => {
                use RuntimeRegisters::*;
                match runtime_registers {
                    MicroframeIndex => "MFINDEX",
                    Interrupter(_, interrupter_registers) => {
                        use InterrupterRegisters::*;
                        match interrupter_registers {
                            Management => "IMAN",
                            Moderation => "IMOD",
                            EventRingSegmentTableSize => "ERSTSZ",
                            EventRingSegmentTableBaseAddress => "ERSTBA",
                            EventRingDequeuePointer => "ERDP",
                        }
                    }
                }
            }
            Doorbell(0) => "Doorbell 0 (Command Ring)",
            Doorbell(_) => "Doorbell (Transfer Ring)",
            ExtCap(extended_capability_registers) => {
                use ExtendedCapabilityRegisters::*;
                match extended_capability_registers {
                    SupportedProtocol1(_) => {
                        "xHCI Supported Protocol Capability (1st DWORD)"
                    }
                    SupportedProtocol2(_) => {
                        "xHCI Supported Protocol Capability (2nd DWORD)"
                    }
                    SupportedProtocol3(_) => {
                        "xHCI Supported Protocol Capability (3rd DWORD)"
                    }
                    SupportedProtocol4(_) => {
                        "xHCI Supported Protocol Capability (4th DWORD)"
                    }
                    Reserved => "xHCI Extended Capability: Reserved",
                }
            }
        }
    }
}
