// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use migrate::PortTypeV1;

use crate::hw::usb::xhci::{
    bits::{self, ring_data::TrbCompletionCode, PortStatusControl},
    registers::PortRegisters,
    rings::producer::event::EventInfo,
    RegRWOpValue, MAX_PORTS,
};

#[must_use]
pub enum PortWrite {
    NoAction,
    BusReset,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct PortId(u8);

impl TryFrom<u8> for PortId {
    type Error = String;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 0 && value <= MAX_PORTS {
            Ok(Self(value - 1))
        } else {
            Err(format!("PortId {value} out of range 1..={MAX_PORTS}"))
        }
    }
}

impl PortId {
    pub fn as_raw_id(&self) -> u8 {
        self.0 + 1
    }
    pub fn as_index(&self) -> usize {
        self.0 as usize
    }
}

#[derive(Copy, Clone, Default)]
pub struct Usb2Port {
    portsc: bits::PortStatusControl,
    portpmsc: bits::PortPowerManagementStatusControlUsb2,
    portli: bits::PortLinkInfoUsb2,
    porthlpmc: bits::PortHardwareLpmControlUsb2,
}

#[derive(Copy, Clone)]
pub struct Usb3Port {
    portsc: bits::PortStatusControl,
    portpmsc: bits::PortPowerManagementStatusControlUsb3,
    portli: bits::PortLinkInfoUsb3,
    porthlpmc: bits::PortHardwareLpmControlUsb3,
}

impl Default for Usb3Port {
    fn default() -> Self {
        Self {
            // xHCI 1.2 sect 4.19.1.2, figure 4-27:
            // the initial state is Disconnected (RxDetect, PP=1)
            portsc: PortStatusControl::default()
                .with_port_link_state(bits::PortLinkState::RxDetect)
                .with_port_power(true),
            portpmsc: Default::default(),
            portli: Default::default(),
            porthlpmc: Default::default(),
        }
    }
}

#[allow(private_bounds)]
pub(super) trait XhciUsbPort: XhciUsbPortPrivate + Send + Sync {
    fn reg_read(&self, regs: PortRegisters) -> RegRWOpValue {
        RegRWOpValue::U32(match regs {
            PortRegisters::PortStatusControl => self.portsc_ref().0,
            PortRegisters::PortPowerManagementStatusControl => {
                self.portpmsc_raw()
            }
            PortRegisters::PortLinkInfo => self.portli_raw(),
            PortRegisters::PortHardwareLpmControl => self.porthlpmc_raw(),
        })
    }

    fn reg_write(
        &mut self,
        value: u32,
        regs: PortRegisters,
        log: &slog::Logger,
    ) -> PortWrite {
        match regs {
            PortRegisters::PortStatusControl => {
                // return: only one of these that can result in a bus reset
                return self.portsc_write(bits::PortStatusControl(value), log);
            }
            PortRegisters::PortPowerManagementStatusControl => {
                self.portpmsc_write(value, log);
            }
            PortRegisters::PortLinkInfo => self.portli_write(value, log),
            PortRegisters::PortHardwareLpmControl => {
                self.porthlpmc_write(value, log)
            }
        }
        PortWrite::NoAction
    }

    /// xHCI 1.2 table 5-27
    fn portsc_write(
        &mut self,
        wo: bits::PortStatusControl,
        log: &slog::Logger,
    ) -> PortWrite {
        let mut retval = PortWrite::NoAction;

        let is_usb3 = self.is_usb3();
        let portsc = self.portsc_mut();
        // software may disable by writing a 1 to PED
        // (*not* enable - only the xHC can do that)
        if wo.port_enabled_disabled() && portsc.port_enabled_disabled() {
            if wo.port_reset() {
                slog::error!(log, "PED=1 and PR=1 written simultaneously - undefined behavior");
            }
            portsc.set_port_enabled_disabled(false);
        }

        // xHCI 1.2 sect 4.19.5
        if wo.port_reset() {
            if !portsc.port_reset() {
                // set the PR bit
                portsc.set_port_reset(true);
                // clear the PED bit to disabled
                portsc.set_port_enabled_disabled(false);
                // tell controller to initiate USB bus reset sequence
                retval = PortWrite::BusReset;
            }
        }

        if wo.port_link_state_write_strobe() {
            // (see xHCI 1.2 sect 4.19, 4.15.2, 4.23.5)
            let to_state = wo.port_link_state();
            let from_state = portsc.port_link_state();
            if to_state != from_state {
                if port_link_state_write_valid(from_state, to_state) {
                    // note: a jump straight from U2 to U3 *technically*
                    // should transition through U0, which we haven't modeled
                    portsc.set_port_link_state(wo.port_link_state());
                    portsc.set_port_link_state_change(true);
                } else {
                    slog::error!(log,
                        "attempted invalid USB{} port transition from {:?} to {:?}",
                        if is_usb3 { '3' } else { '2' },
                        from_state, to_state
                    );
                }
            }

            // PP shouldn't have to be implemented because PPC in HCCPARAMS1 unset,
            // but we must allow it to be set so software can change port state
            // (xHCI 1.2 sect 5.4.8)
            portsc.set_port_power(wo.port_power());
        }

        if is_usb3 {
            use bits::PortLinkState::*;
            // xHCI 1.2 figure 4-27
            // xHCI 1.2 sect 4.19.1.2.1
            if wo.port_enabled_disabled() {
                // transition from any state except powered-off to Disabled
            }
            // xHCI 1.2 sect 4.19.1.2.2
            if !wo.port_power() {
                // write PP=0 => transition to Powered-off
                portsc.set_port_link_state(Disabled);
            } else {
                // write PP=1 => transition to Disconnected
                portsc.set_port_link_state(RxDetect);
            }
        } else {
            use bits::PortLinkState::*;
            // xHCI 1.2 figure 4-25
            if wo.port_power() {
                portsc.set_port_power(true);
                // write PP=1 transitions from Powered-off to Disconnected
                if portsc.port_link_state() == Disabled {
                    portsc.set_port_link_state(RxDetect);
                }
            } else {
                portsc.set_port_power(false);
                // write PP=0 transitions any state to Powered-off
                portsc.set_port_link_state(Disabled);
            }

            // xHCI 1.2 sect 4.19.1.1.6:
            // write PED=1 => transition from Enabled to Disabled
            if wo.port_enabled_disabled()
                && matches!(
                    portsc.port_link_state(),
                    U0 | U2 | U3Suspended | Resume
                )
            {
                portsc.set_port_link_state(Polling);
            }
        }

        // PIC not implemented because PIND in HCCPARAMS1 unset

        // following are RW1CS fields (software clears by writing 1)

        if wo.connect_status_change() {
            portsc.set_connect_status_change(false);
        }

        if wo.port_enabled_disabled_change() {
            portsc.set_port_enabled_disabled_change(false);
        }

        if is_usb3 && wo.warm_port_reset_change() {
            portsc.set_warm_port_reset_change(false);
        }

        if wo.overcurrent_change() {
            portsc.set_overcurrent_change(false);
        }

        if wo.port_reset_change() {
            portsc.set_port_reset_change(false);
        }

        if wo.port_link_state_change() {
            portsc.set_port_link_state_change(false);
        }

        if wo.port_config_error_change() {
            portsc.set_port_config_error_change(false);
        }

        // following are RWS (not RW1CS)
        portsc.set_wake_on_connect_enable(wo.wake_on_connect_enable());
        portsc.set_wake_on_disconnect_enable(wo.wake_on_disconnect_enable());
        portsc.set_wake_on_overcurrent_enable(wo.wake_on_overcurrent_enable());

        if is_usb3 && wo.warm_port_reset() {
            // TODO: initiate USB3 warm port reset sequence
            // (implement whenever any USB devices exist)
            portsc.set_port_reset(true);
        }

        retval
    }

    // entirely different registers between USB2/3
    fn portpmsc_write(&mut self, value: u32, log: &slog::Logger);
    fn portli_write(&mut self, value: u32, log: &slog::Logger);
    fn porthlpmc_write(&mut self, value: u32, log: &slog::Logger);

    /// Update the given port's PORTSC register from the xHC itself.
    /// Returns Some(EventInfo) with a Port Status Change Event to be
    /// enqueued in the Event Ring if the PSCEG signal changes from 0 to 1.
    fn xhc_update_portsc(
        &mut self,
        update: &dyn Fn(&mut bits::PortStatusControl),
        port_id: PortId,
    ) -> Option<EventInfo> {
        let is_usb3 = self.is_usb3();
        let portsc_before = *self.portsc_ref();

        let portsc = self.portsc_mut();
        update(portsc);

        // any change to CCS or CAS => force CSC to 1
        if portsc_before.current_connect_status()
            != portsc.current_connect_status()
            || portsc_before.cold_attach_status() != portsc.cold_attach_status()
        {
            portsc.set_connect_status_change(true);
        }

        if is_usb3 {
            todo!("usb3 portsc controller-side change")
            // TODO: if Hot Reset transitioned to Warm Reset, set WRC to 1
        } else {
            // for concise readability
            use bits::PortLinkState::*;

            // xHCI 1.2 sect 4.19.1.1
            if portsc_before.current_connect_status()
                && !portsc.current_connect_status()
                && matches!(
                    portsc.port_link_state(),
                    Polling | U0 | U2 | U3Suspended | Resume
                )
            {
                portsc.set_port_link_state(RxDetect);
            } else if !portsc_before.current_connect_status()
                && portsc.current_connect_status()
                && portsc.port_link_state() == RxDetect
            {
                portsc.set_port_link_state(Polling);
            }

            // xHCI 1.2 sect 4.19.1.1.4:
            // PR=0 => advance to Enabled, set PED and PRC to 1
            if portsc_before.port_reset() && !portsc.port_reset() {
                portsc.set_port_link_state(U0);
                portsc.set_port_enabled_disabled(true);
                portsc.set_port_reset_change(true);
            }
        }

        let psceg_before = portsc_before.port_status_change_event_generation();
        let psceg_after = portsc.port_status_change_event_generation();
        if psceg_after && !psceg_before {
            Some(EventInfo::PortStatusChange {
                port_id,
                completion_code: TrbCompletionCode::Success,
            })
        } else {
            None
        }
    }

    fn import(
        &mut self,
        value: &migrate::XhcUsbPortV1,
    ) -> Result<(), crate::migrate::MigrateStateError> {
        let migrate::XhcUsbPortV1 {
            port_type,
            portsc,
            portpmsc,
            portli,
            porthlpmc,
        } = value;
        match port_type {
            PortTypeV1::Usb2 if !self.is_usb3() => (),
            PortTypeV1::Usb3 if self.is_usb3() => (),
            _ => {
                return Err(crate::migrate::MigrateStateError::ImportFailed(
                    format!("wrong port type for this port: {port_type:?}"),
                ));
            }
        }
        self.portsc_mut().0 = *portsc;
        *self.portpmsc_mut() = *portpmsc;
        *self.portli_mut() = *portli;
        *self.porthlpmc_mut() = *porthlpmc;
        Ok(())
    }

    fn export(&self) -> migrate::XhcUsbPortV1 {
        migrate::XhcUsbPortV1 {
            port_type: if self.is_usb3() {
                PortTypeV1::Usb3
            } else {
                PortTypeV1::Usb2
            },
            portsc: self.portsc_ref().0,
            portpmsc: self.portpmsc_raw(),
            portli: self.portli_raw(),
            porthlpmc: self.porthlpmc_raw(),
        }
    }
}

trait XhciUsbPortPrivate {
    fn is_usb3(&self) -> bool;

    fn portsc_ref(&self) -> &bits::PortStatusControl;
    fn portsc_mut(&mut self) -> &mut bits::PortStatusControl;
    fn portpmsc_raw(&self) -> u32;
    fn portli_raw(&self) -> u32;
    fn porthlpmc_raw(&self) -> u32;
    fn portpmsc_mut(&mut self) -> &mut u32;
    fn portli_mut(&mut self) -> &mut u32;
    fn porthlpmc_mut(&mut self) -> &mut u32;
}

impl XhciUsbPortPrivate for Usb2Port {
    fn is_usb3(&self) -> bool {
        false
    }

    fn portsc_ref(&self) -> &bits::PortStatusControl {
        &self.portsc
    }
    fn portsc_mut(&mut self) -> &mut bits::PortStatusControl {
        &mut self.portsc
    }
    fn portpmsc_raw(&self) -> u32 {
        self.portpmsc.0
    }
    fn portli_raw(&self) -> u32 {
        self.portli.0
    }
    fn porthlpmc_raw(&self) -> u32 {
        self.porthlpmc.0
    }
    fn portpmsc_mut(&mut self) -> &mut u32 {
        &mut self.portpmsc.0
    }
    fn portli_mut(&mut self) -> &mut u32 {
        &mut self.portli.0
    }
    fn porthlpmc_mut(&mut self) -> &mut u32 {
        &mut self.porthlpmc.0
    }
}

impl XhciUsbPort for Usb2Port {
    fn portpmsc_write(&mut self, value: u32, log: &slog::Logger) {
        let portpmsc = bits::PortPowerManagementStatusControlUsb2(value);
        slog::debug!(log, "{portpmsc:?}");
        self.portpmsc.set_remote_wake_enable(portpmsc.remote_wake_enable());
        self.portpmsc.set_best_effort_service_latency(
            portpmsc.best_effort_service_latency(),
        );
        self.portpmsc.set_l1_device_slot(portpmsc.l1_device_slot());
        self.portpmsc.set_hardware_lpm_enable(portpmsc.hardware_lpm_enable());
        self.portpmsc.set_port_test_control(portpmsc.port_test_control());
        // xHCI 1.2 sect 4.19.1.1.1: write to PORTPMSC Test Mode > 0
        // transitions from Powered-off state to Test Mode state
        if portpmsc.port_test_control()
            != bits::PortTestControl::TestModeNotEnabled
        {
            if self.portsc.port_link_state() == bits::PortLinkState::Disabled {
                self.portsc.set_port_link_state(bits::PortLinkState::TestMode);
            }
        }
    }

    fn portli_write(&mut self, _value: u32, _log: &slog::Logger) {
        // (in USB2 PORTLI is reserved)
    }

    fn porthlpmc_write(&mut self, value: u32, log: &slog::Logger) {
        let porthlpmc = bits::PortHardwareLpmControlUsb2(value);
        slog::debug!(log, "{porthlpmc:?}");
        self.porthlpmc.set_host_initiated_resume_duration_mode(
            porthlpmc.host_initiated_resume_duration_mode(),
        );
        self.porthlpmc.set_l1_timeout(porthlpmc.l1_timeout());
        self.porthlpmc.set_best_effort_service_latency_deep(
            porthlpmc.best_effort_service_latency_deep(),
        );
    }
}

impl XhciUsbPortPrivate for Usb3Port {
    fn is_usb3(&self) -> bool {
        true
    }

    fn portsc_ref(&self) -> &bits::PortStatusControl {
        &self.portsc
    }
    fn portsc_mut(&mut self) -> &mut bits::PortStatusControl {
        &mut self.portsc
    }
    fn portpmsc_raw(&self) -> u32 {
        self.portpmsc.0
    }
    fn portli_raw(&self) -> u32 {
        self.portli.0
    }
    fn porthlpmc_raw(&self) -> u32 {
        self.porthlpmc.0
    }
    fn portpmsc_mut(&mut self) -> &mut u32 {
        &mut self.portpmsc.0
    }
    fn portli_mut(&mut self) -> &mut u32 {
        &mut self.portli.0
    }
    fn porthlpmc_mut(&mut self) -> &mut u32 {
        &mut self.porthlpmc.0
    }
}

impl XhciUsbPort for Usb3Port {
    fn portpmsc_write(&mut self, value: u32, _log: &slog::Logger) {
        // all RWS and RW fields
        self.portpmsc = bits::PortPowerManagementStatusControlUsb3(value);
    }

    fn portli_write(&mut self, value: u32, _log: &slog::Logger) {
        let portli = bits::PortLinkInfoUsb3(value);
        // only field writable by software
        self.portli.set_link_error_count(portli.link_error_count());
    }

    fn porthlpmc_write(&mut self, _value: u32, _log: &slog::Logger) {
        // (in USB3 PORTHLPMC is reserved)
    }
}

fn port_link_state_write_valid(
    from_state: bits::PortLinkState,
    to_state: bits::PortLinkState,
) -> bool {
    use bits::PortLinkState::*;
    // see xHCI 1.2 table 5-27 and section 4.19.1.1
    match (from_state, to_state) {
        // xHCI 1.2 sect 4.19.1.2.1, fig 4-27 (usb3 root hub port diagram)
        // (outside of the usb2 port enabled substate, but implemented identically)
        (Disabled, RxDetect) => true,

        // software may write 0 (U0), 2 (U2), 3 (U3), 5 (RxDetect), 10 (ComplianceMode), 15 (Resume).
        // software writing 1, 4, 6-9, 11-14 (the below variants) shall be ignored.
        (
            _,
            U1 | Disabled | Inactive | Polling | Recovery | HotReset | TestMode,
        ) => false,
        // writes to PLC don't change anything outside of the usb2 port enabled substate
        (
            U1 | Disabled | Inactive | Polling | Recovery | HotReset | TestMode,
            _,
        ) => false,
        // we haven't implemented USB3 ports yet
        (_, RxDetect | ComplianceMode) | (RxDetect | ComplianceMode, _) => {
            false
        }
        // xHCI 1.2 fig 4-26 (usb2 port enabled subspace diagram)
        (U0, U2 | U3Suspended) => true,
        (U0, Resume) => false,
        (U2, U0 | U3Suspended) => true,
        (U2, Resume) => false,
        (U3Suspended, U0 | U2) => false,
        (U3Suspended, Resume) => true,
        (Resume, U0) => true,
        (Resume, U2 | U3Suspended) => false,
        // ignore unchanged
        (U0, U0) | (U2, U2) | (U3Suspended, U3Suspended) | (Resume, Resume) => {
            false
        }
        // reserved
        (Reserved12 | Reserved13 | Reserved14, _)
        | (_, Reserved12 | Reserved13 | Reserved14) => false,
    }
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug)]
    pub enum PortTypeV1 {
        Usb2,
        Usb3,
    }

    #[derive(Serialize, Deserialize)]
    pub struct XhcUsbPortV1 {
        pub port_type: PortTypeV1,
        pub portsc: u32,
        pub portpmsc: u32,
        pub portli: u32,
        pub porthlpmc: u32,
    }
}
