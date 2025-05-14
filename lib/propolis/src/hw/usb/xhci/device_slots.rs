// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module implements the host controller's behaviors in response to
//! TRBs received on the [Command Ring]. For an overview of how Command TRBs
//! transition the [SlotState]s of [SlotContext]s, see xHCI 1.2 figure 4-2.
//!
//! [Command Ring]: crate::hw::usb::xhci::rings::consumer::command

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use zerocopy::{FromBytes, IntoBytes};

use super::bits::device_context::{
    EndpointContext, EndpointState, InputControlContext, SlotContext,
    SlotContextFirst, SlotState,
};
use super::bits::ring_data::TrbCompletionCode;
#[cfg(doc)]
use super::bits::PortStatusControl;
use super::port::PortId;
use super::rings::consumer::transfer::TransferRing;
use super::{MAX_DEVICE_SLOTS, MAX_PORTS};

use crate::common::GuestAddr;
use crate::hw::pci;
use crate::hw::usb::usbdev::{UsbDevice, UsbDeviceType};
use crate::hw::usb::xhci::interrupter::EventSender;
use crate::hw::usb::xhci::rings::producer::event::EventInfo;
use crate::vmm::time::VmGuestTime;
use crate::vmm::MemCtx;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_endpoint_state(slot_id: u8, endpoint_id: u8, state: u8) {}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot use 0-value SlotId as array index")]
    SlotIdZero,
    #[error("Cannot use 0-value EndpointId as context offset")]
    EndpointIdZero,
    #[error("{0:?} greater than max device slots ({MAX_DEVICE_SLOTS})")]
    SlotIdBounds(SlotId),
    #[error("USB device not found in {0:?}")]
    NoUsbDevInSlot(SlotId),
    #[error("{0:?} not enabled")]
    SlotNotEnabled(SlotId),
    #[error("{0:?} does not have {1:?}")]
    EndpointNotPresent(SlotId, EndpointId),
    #[error("No port associated with {0:?}")]
    NoPortAssociatedWithSlot(SlotId),
    #[error(
        "Could not set TR Dequeue Pointer {0:x?} and cycle state {1}: {2}"
    )]
    SetTRDPFailed(GuestAddr, bool, super::rings::consumer::Error),
    #[error("{0:?} inactive due to CONFIG register max slots being {1}")]
    SlotInactive(SlotId, u8),
    #[error("Device Context accessed before DCBAAP set")]
    DevContextPointerUnset,
    #[error("Failed to access guest memory")]
    MemCtx,
    #[error("rang Doorbell for {0:?} {1:?}, whose state was {2:?}")]
    EndpointInappropriateState(SlotId, EndpointId, EndpointState),
    #[error("rang Doorbell for {0:?} {1:?}, whose state was invalid")]
    EndpointInvalidState(SlotId, EndpointId),
}
pub type Result<T> = core::result::Result<T, Error>;

/// The index of a Device Slot within the Device Context Base Address Array.
/// Valid values are 1 through MAX_DEVICE_SLOTS-1.
/// When a USB device is attached, its Slot ID is assigned when the guest OS
/// issues an Enable Slot Command TRB on the Command Ring.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct SlotId(u8);

impl From<u8> for SlotId {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

impl From<SlotId> for u8 {
    fn from(value: SlotId) -> Self {
        value.0
    }
}

impl SlotId {
    pub fn as_index(&self) -> Result<usize> {
        if self.0 <= MAX_DEVICE_SLOTS {
            (self.0 as usize).checked_sub(1).ok_or(Error::SlotIdZero)
        } else {
            Err(Error::SlotIdBounds(*self))
        }
    }
    pub fn from_index(index: usize) -> Result<Self> {
        let slot_id = Self(index as u8 + 1);
        if index < MAX_DEVICE_SLOTS as usize {
            Ok(slot_id)
        } else {
            Err(Error::SlotIdBounds(slot_id))
        }
    }
}

/// The ID of an Endpoint, also used as its Device Context Index (DCI).
/// (See xHCI 1.2 sect 4.5.1.)
/// Valid values are 0 through 31 (specified as a 5-bit field in many TRBs).
/// The specific Endpoint IDs that exist (and their respective purposes) will
/// vary for different USB devices. The Default Control Endpoint, with DCI 1
/// (though often referred to as "endpoint number 0" or "EP0" by the spec --
/// DCI 0 refers to the Slot Context, per xHCI 1.2 figure 4-4), is enabled as
/// soon as the Address Device Command has been executed.
/// Other Endpoints are enabled/disabled by the Configure Endpoint Command.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct EndpointId(u8);

// for bitstruct conversion, from 5-bit fields that will never be >31
impl From<u8> for EndpointId {
    fn from(value: u8) -> Self {
        if value > 31 {
            panic!("EndpointId must be <= 31, got {value}")
        } else {
            Self(value)
        }
    }
}

impl From<EndpointId> for u8 {
    fn from(value: EndpointId) -> Self {
        value.0
    }
}

/// Guard around an guest address to ensure modifications are written
/// back to guest memory after modification.
struct MemCtxValue<'a, T: Copy + FromBytes> {
    value: T,
    addr: GuestAddr,
    memctx: &'a MemCtx,
}

impl<T: Copy + FromBytes + core::fmt::Debug> core::fmt::Debug
    for MemCtxValue<'_, T>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        core::fmt::Debug::fmt(&self.value, f)
    }
}

impl<'a, T: Copy + FromBytes + IntoBytes> MemCtxValue<'a, T> {
    pub fn new(addr: GuestAddr, memctx: &'a MemCtx) -> Result<Self> {
        memctx
            .read(addr)
            .map(move |value| Self { value: *value, addr, memctx })
            .ok_or(Error::MemCtx)
    }
    pub fn mutate(&mut self, mut f: impl FnMut(&mut T)) -> bool {
        f(&mut self.value);
        self.memctx.write(self.addr, &self.value)
    }
}

impl<T: Copy + FromBytes> Deref for MemCtxValue<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// Tracks xHC resources corresponding to an entry in the DeviceSlotTable
struct DeviceSlot {
    slot_id: SlotId,
    /// [TransferRing]s of the Endpoints that have been enabled for the device
    /// assigned to this slot.
    endpoints: HashMap<EndpointId, TransferRing>,
    /// When a device has been assigned to this slot, this contains the [PortId]
    /// of the USB port to which the device was originally attached.
    port_address: Option<PortId>,
}
impl DeviceSlot {
    fn new(slot_id: SlotId) -> Self {
        Self { slot_id, endpoints: HashMap::new(), port_address: None }
    }

    /// Begin tracking the guest-allocated [TransferRing] corresponding to the
    /// given [EndpointId] with the TRDP and cycle state given in the
    /// [EndpointContext], such that doorbell rings on this endpoint can be
    /// serviced.
    fn set_endpoint_transfer_ring(
        &mut self,
        endpoint_id: EndpointId,
        ep_ctx: EndpointContext,
    ) {
        self.endpoints.insert(
            endpoint_id,
            // unwrap: tr_dequeue_pointer's lower 4 bits are 0, will always be TRB-aligned
            TransferRing::new(
                ep_ctx.tr_dequeue_pointer(),
                ep_ctx.dequeue_cycle_state(),
            )
            .unwrap(),
        );
    }

    /// Remove the [TransferRing] for the given [EndpointId], such as when the
    /// endpoint is disabled.
    fn unset_endpoint_transfer_ring(&mut self, endpoint_id: EndpointId) {
        self.endpoints.remove(&endpoint_id);
    }

    fn transfer_ring_mut(
        &mut self,
        endpoint_id: &EndpointId,
    ) -> Result<&mut TransferRing> {
        self.endpoints.get_mut(endpoint_id).ok_or_else(|| {
            Error::EndpointNotPresent(self.slot_id, *endpoint_id)
        })
    }

    fn import(
        &mut self,
        value: &migrate::DeviceSlotV1,
    ) -> core::result::Result<(), crate::migrate::MigrateStateError> {
        let migrate::DeviceSlotV1 { endpoints, port_address } = value;
        self.port_address = port_address
            .map(PortId::try_from)
            .transpose()
            .map_err(crate::migrate::MigrateStateError::ImportFailed)?;
        for ep_id in
            self.endpoints.keys().copied().map(u8::from).collect::<Vec<_>>()
        {
            if !endpoints.contains_key(&ep_id) {
                self.endpoints.remove(&EndpointId::from(ep_id));
            }
        }
        for (ep_id, ep) in endpoints {
            self.endpoints.insert(
                EndpointId::from(*ep_id),
                TransferRing::try_from(ep).map_err(|e| {
                    crate::migrate::MigrateStateError::ImportFailed(format!(
                        "{e:?}"
                    ))
                })?,
            );
        }
        Ok(())
    }

    fn export(&self) -> migrate::DeviceSlotV1 {
        let Self { slot_id: _, endpoints, port_address } = self;
        migrate::DeviceSlotV1 {
            endpoints: endpoints
                .iter()
                .map(|(ep_id, ep)| (u8::from(*ep_id), ep.export()))
                .collect(),
            port_address: port_address.map(|port_id| port_id.as_raw_id()),
        }
    }
}

/// Contains the attached [UsbDevice]s, and is responsible for assigning
/// them [SlotId]s. Implements the execution of each Command Ring TRBs as
/// outlined in xHCI 1.2 section 4.6.
pub struct DeviceSlotTable {
    /// Device Context Base Address Array Pointer (DCBAAP)
    ///
    /// Points to an array of address pointers referencing the device context
    /// structures for each attached device.
    ///
    /// See xHCI 1.2 Section 5.4.6
    dcbaap: Option<GuestAddr>,
    slots: Vec<Option<DeviceSlot>>,
    config_max_slots: u8,
    port_devs: [Option<Box<dyn UsbDevice>>; MAX_PORTS as usize],
    log: slog::Logger,
}

impl DeviceSlotTable {
    pub fn new(log: slog::Logger) -> Self {
        Self {
            dcbaap: None,
            slots: vec![],
            config_max_slots: 0,
            // seemingly asinine, but Box<dyn UsbDevice>: !Copy,
            // which invalidates [None; _] syntax
            port_devs: [None, None, None, None, None, None, None, None],
            log,
        }
    }

    /// Called when the CONFIG register is written
    pub fn set_max_slots(&mut self, max_device_slots_enabled: u8) {
        self.config_max_slots = max_device_slots_enabled;
    }

    // Command TRBs implemented here are done with a priority on making the line
    // code closely resemble the outlines given in each corresponding xHCI 1.2
    // subsection 4.6.*, generally introduced in the text by "When a {} Command
    // is executed by the xHC it shall perform the following operations:", such
    // that aspirationally, correctness-to-the-spec (and mistakes/divergences)
    // will be obvious to anyone comparing them.

    /// Sets the Device Context Base Address Array Pointer.
    pub fn set_dcbaap(&mut self, addr: GuestAddr) {
        self.dcbaap = Some(addr)
    }
    /// Returns the Device Context Base Address Array Pointer, if set.
    pub fn dcbaap(&self) -> Option<&GuestAddr> {
        self.dcbaap.as_ref()
    }

    /// Assigns the given [UsbDevice] to the given [PortId], returning the
    /// device that previously resided there, if any.
    pub fn attach_to_root_hub_port_address(
        &mut self,
        port_id: PortId,
        usb_dev: Box<dyn UsbDevice>,
    ) -> Option<Box<dyn UsbDevice>> {
        self.port_devs[port_id.as_index()].replace(usb_dev)
    }

    /// Removes the [UsbDevice]s from their attached ports to be replaced into
    /// the xHC's queued device connections, as part of host controller reset.
    pub fn detach_all_for_reset(
        &mut self,
    ) -> impl Iterator<Item = (PortId, Box<dyn UsbDevice>)> + '_ {
        self.port_devs.iter_mut().enumerate().flat_map(|(i, opt_dev)| {
            opt_dev
                .take()
                .map(|dev| (PortId::try_from(i as u8 + 1).unwrap(), dev))
        })
    }

    /// Access the [UsbDevice] for a given [SlotId], i.e. when servicing a
    /// doorbell register write.
    pub fn usbdev_for_slot(
        &mut self,
        slot_id: SlotId,
    ) -> Result<&mut Box<dyn UsbDevice>> {
        // xHCI 1.2 table 5-26: "A value of 16 specifies that Device Slots
        // 1 to 16 are active. A value of '0' disables all Device Slots. A
        // disabled Device Slot shall not respond to Doorbell Register
        // references."
        if u8::from(slot_id) > self.config_max_slots {
            return Err(Error::SlotInactive(slot_id, self.config_max_slots));
        }
        let slot = self.slot_mut(slot_id)?;
        let idx = slot
            .port_address
            .ok_or(Error::NoPortAssociatedWithSlot(slot_id))?;
        self.port_devs[idx.as_index()]
            .as_mut()
            .ok_or(Error::NoUsbDevInSlot(slot_id))
    }

    fn slot(&self, slot_id: SlotId) -> Result<&DeviceSlot> {
        self.slots
            .get(slot_id.as_index()?)
            .and_then(|res| res.as_ref())
            .ok_or(Error::SlotNotEnabled(slot_id))
    }

    fn slot_mut(&mut self, slot_id: SlotId) -> Result<&mut DeviceSlot> {
        self.slots
            .get_mut(slot_id.as_index()?)
            .and_then(|res| res.as_mut())
            .ok_or(Error::SlotNotEnabled(slot_id))
    }

    fn endpoint_context(
        slot_addr: GuestAddr,
        endpoint_id: EndpointId,
        memctx: &'_ MemCtx,
    ) -> Result<MemCtxValue<'_, EndpointContext>> {
        const { assert!(size_of::<SlotContext>() == size_of::<EndpointContext>()) };
        MemCtxValue::new(
            slot_addr.offset::<SlotContext>(1).offset::<EndpointContext>(
                u8::from(endpoint_id)
                    .checked_sub(1)
                    .ok_or(Error::EndpointIdZero)? as usize,
            ),
            memctx,
        )
    }

    /// Returns the address of the Device Context for the given [SlotId], i.e.
    /// that slot's position in the array of pointers at the address written to
    /// the DCBAAP register.
    fn dev_context_addr(
        &self,
        slot_id: SlotId,
        memctx: &MemCtx,
    ) -> Result<GuestAddr> {
        self.dcbaap
            .as_ref()
            .and_then(|base_ptr| {
                memctx
                    .read::<u64>(
                        // lower 6 bits are reserved (xHCI 1.2 table 6-2)
                        // software is supposed to clear them to 0, but
                        // let's double-tap for safety's sake (i.e. paranoid
                        // far-future-proofing should they become un-reserved)
                        (*base_ptr & !0b11_1111)
                            // NOTE: index 0 *is* reserved in DCBAA, so we don't SlotId::as_index()
                            .offset::<u64>(u8::from(slot_id) as usize),
                    )
                    .map(|x| GuestAddr(*x))
            })
            .ok_or(Error::DevContextPointerUnset)
    }

    /// xHCI 1.2 sect 4.6.3: Enable Slot Command.
    ///
    /// Issued by guest after we send it an [EventInfo::PortStatusChange]
    /// to indicate a new USB device has been attached, such that we
    /// allocate it a valid [SlotId].
    pub fn enable_slot(&mut self, slot_type: u8) -> Option<SlotId> {
        // USB protocol slot type is 0 (xHCI 1.2 section 7.2.2.1.4)
        if slot_type != 0 {
            return None;
        }
        let slot_id_opt = self
            .slots
            .iter()
            .position(Option::is_none)
            .map(|i| SlotId::from(i as u8 + 1))
            .or_else(|| {
                if self.slots.len() < MAX_DEVICE_SLOTS as usize
                    && self.slots.len() < self.config_max_slots as usize
                {
                    self.slots.push(None);
                    Some(SlotId::from(self.slots.len() as u8))
                } else {
                    None
                }
            });
        if let Some(slot_id) = slot_id_opt {
            self.slots[slot_id.as_index().unwrap()] =
                Some(DeviceSlot::new(slot_id));
        }
        slot_id_opt
    }

    /// xHCI 1.2 sect 4.6.4: Disable Slot Command.
    ///
    /// Issued by guest to free the given [SlotId], such as when the USB device
    /// associated is disconnected, or the device is otherwise expllicitly
    /// disabled by the OS.
    pub fn disable_slot(
        &mut self,
        slot_id: SlotId,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }

        // slot ctx is first element of dev context table
        let slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let mut slot_ctx =
            MemCtxValue::<SlotContextFirst>::new(slot_addr, memctx)?;
        slot_ctx.mutate(|ctx| ctx.set_slot_state(SlotState::DisabledEnabled));

        // unwraps: self.slot(slot_id).is_ok(), above
        let slot_idx = slot_id.as_index().unwrap();
        for endpoint_id in
            self.slots[slot_idx].as_ref().unwrap().endpoints.keys()
        {
            // xHCI 1.2 figure 4-5: "The Disable Slot Command shall
            // transition all endpoints of a Device Slot, including
            // the Default Control Endpoint, from any state to the
            // Disabled state."
            // (this is not included in the outline in 4.6.4, unless
            // we're to take a particularly flexible reading of "disable
            // the doorbell register for the slot" to include the guest-
            // memory-resident endpoint context bookkeeping too)
            let mut ep_ctx =
                Self::endpoint_context(slot_addr, *endpoint_id, memctx)?;
            if ep_ctx.endpoint_state() != EndpointState::Disabled {
                probes::xhci_endpoint_state!(|| (
                    u8::from(slot_id),
                    u8::from(*endpoint_id),
                    EndpointState::Disabled as u8
                ));
            }
            ep_ctx
                .mutate(|ctx| ctx.set_endpoint_state(EndpointState::Disabled));
        }

        // terminate any transfers on the slot
        // free any internal resources associated with the slot
        // internally flag the slot as available for subsequent enable_slot
        self.slots[slot_idx] = None;
        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.5: Address Device Command.
    ///
    /// During device initialization, this command is typically issued twice,
    /// after the Enable Slot Command: first with `block_set_address_request`
    /// (BSR) set to 1 to transition the slot from [SlotState::DisabledEnabled]
    /// to [SlotState::Default], then with BSR set to 0 to transition it from
    /// [SlotState::Default] to [SlotState::Addressed]. Regardless of BSR, the
    /// input [SlotContext] and DCE [EndpointContext] are copied into the
    /// device context, and DCE's state is set to [EndpointState::Running].
    // (implementation following per outline on pg. 113)
    pub fn address_device(
        &mut self,
        slot_id: SlotId,
        input_context_ptr: GuestAddr,
        block_set_address_request: bool,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }

        // xHCI 1.2 Figure 6-5: differences between input context indeces
        // and device context indeces.

        let out_slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let out_ep0_addr = out_slot_addr.offset::<SlotContext>(1);

        let in_slot_addr = input_context_ptr.offset::<InputControlContext>(1);
        let in_ep0_addr = in_slot_addr.offset::<SlotContext>(1);

        let mut slot_ctx =
            memctx.read::<SlotContext>(in_slot_addr).ok_or(Error::MemCtx)?;
        let mut ep0_ctx =
            memctx.read::<EndpointContext>(in_ep0_addr).ok_or(Error::MemCtx)?;

        // we'll just use the root hub port number as the USB address for
        // whatever's in this slot
        if let Ok(port_id) = slot_ctx.root_hub_port_number() {
            // unwrap: we've checked is_none already
            self.slot_mut(slot_id).unwrap().port_address = Some(port_id);
        }

        let usb_addr = if block_set_address_request {
            if matches!(slot_ctx.slot_state(), SlotState::DisabledEnabled) {
                // set output slot context state to default
                slot_ctx.set_slot_state(SlotState::Default);

                0 // because BSR, just set field to 0 in slot ctx
            } else {
                return Ok(TrbCompletionCode::ContextStateError);
            }
        } else if matches!(
            slot_ctx.slot_state(),
            SlotState::DisabledEnabled | SlotState::Default
        ) {
            // we'll just use the root hub port number as our USB address
            if let Some(port_id) =
                self.slot(slot_id).ok().and_then(|slot| slot.port_address)
            {
                // issue 'set address' to USB device itself,
                // which configures its Default Control Endpoint
                self.usbdev_for_slot(slot_id)?.set_address(slot_id, port_id);

                // set output slot context state to addressed
                slot_ctx.set_slot_state(SlotState::Addressed);

                port_id.as_raw_id()
            } else {
                // root hub port ID in slot ctx not in range of our ports
                return Ok(TrbCompletionCode::ContextStateError);
            }
        } else {
            return Ok(TrbCompletionCode::ContextStateError);
        };

        // set usb device address in output slot ctx to chosen addr (or 0 if BSR)
        slot_ctx.set_usb_device_address(usb_addr);
        // copy input slot ctx to output slot ctx
        memctx.write(out_slot_addr, &*slot_ctx);

        // set output ep0 state to running
        ep0_ctx.set_endpoint_state(EndpointState::Running);
        // copy input ep0 ctx to output ep0 ctx
        memctx.write(out_ep0_addr, &*ep0_ctx);
        probes::xhci_endpoint_state!(|| (
            u8::from(slot_id),
            1,
            ep0_ctx.endpoint_state() as u8
        ));

        slog::trace!(
            self.log,
            "slot_ctx: in@{:#x} out@{:#x} {slot_ctx:?}",
            in_slot_addr.0,
            out_slot_addr.0
        );
        slog::trace!(
            self.log,
            "ep0_ctx: in@{:#x} out@{:#x} {ep0_ctx:?}",
            in_ep0_addr.0,
            out_ep0_addr.0
        );

        // unwrap: we've checked self.slot() at function begin,
        // we just can't hold a &mut for the whole duration
        let device_slot = self.slot_mut(slot_id).unwrap();

        // add default control endpoint to scheduling list
        device_slot.set_endpoint_transfer_ring(EndpointId::from(1), *ep0_ctx);

        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.6: Configure Endpoint Command.
    /// (see also sections 3.3.5 and 6.2.3.2, and figure 4-3)
    ///
    /// Enable and disable USB endpoints specified by Add/Drop Context flags,
    /// along with their corresponding Transfer Rings (according to the TRDP
    /// and CCS of the input endpoint contexts provided).
    // NOTE: if we were concerned with bandwidth/resource limits
    // as real hardware is, this function would
    // - add/subtract resources allocated to the endpoint from a
    //   Resource Required variable
    // - if endpoint is periodic, add/subtract bandwidth allocated to the
    //   endpoint from a Bandwidth Required variable
    // in various points throughout the function where contexts are added
    // and dropped. these steps (as outlined in xHCI 1.2 sect 4.6.6)
    // are elided, since we're a virtual device not *as* subject to limitations
    // of physical interconnects as a physical xHC would be.
    pub fn configure_endpoint(
        &mut self,
        input_context_ptr: GuestAddr,
        slot_id: SlotId,
        deconfigure: bool,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        // following xHC behavior described in xHCI 1.2 sect 4.6.6:
        // if not previously enabled by an Enable Slot command
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }

        // retrieve the output device context of the selected device slot
        let out_slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let mut out_slot_ctx =
            MemCtxValue::<SlotContext>::new(out_slot_addr, memctx)?;

        // if output dev context slot state is not addressed or configured
        if !matches!(
            out_slot_ctx.slot_state(),
            SlotState::Addressed | SlotState::Configured
        ) {
            return Ok(TrbCompletionCode::ContextStateError);
        }

        // if the output slot state is configured
        if deconfigure {
            // setting deconfigure (DC) is equivalent to setting Input Context
            // Drop Context flags 2..=31 to 1 and Add Context flags 2..=31 to 0.
            // if DC=1, Input Context Pointer field shall be *ignored* by the xHC,
            // the Output Slot Context 'Context Entries' field shall be set to 1.
            out_slot_ctx.mutate(|out_ctx| {
                out_ctx.set_context_entries(1);
            });
            if out_slot_ctx.slot_state() == SlotState::Configured {
                // for each endpoint context (except DCE) not in disabled state:
                for i in (2..=31).map(EndpointId::from) {
                    let mut out_ep_ctx =
                        Self::endpoint_context(out_slot_addr, i, memctx)?;
                    if out_ep_ctx.endpoint_state() != EndpointState::Disabled {
                        // set output EP State field to disabled
                        out_ep_ctx.mutate(|ctx| {
                            ctx.set_endpoint_state(EndpointState::Disabled)
                        });
                        probes::xhci_endpoint_state!(|| (
                            u8::from(slot_id),
                            u8::from(i),
                            out_ep_ctx.endpoint_state() as u8
                        ));
                        // xHCI 1.2 sect 4.5.3.5: only doorbell enabled when
                        // slot is in Addressed is control EP 0.
                        // unwrap: returned early if self.slot(slot_id) was none
                        self.slot_mut(slot_id)
                            .unwrap()
                            .unset_endpoint_transfer_ring(i);
                    }
                }
                // set Slot State in output slot context to Addressed
                out_slot_ctx
                    .mutate(|ctx| ctx.set_slot_state(SlotState::Addressed));
            }
        }
        // if output slot state is addressed or configured and DC=0
        // (for slot state, we've already returned ContextStateError otherwise)
        else {
            let input_ctx = *memctx
                .read::<InputControlContext>(input_context_ptr)
                .ok_or(Error::MemCtx)?;

            let in_slot_addr =
                input_context_ptr.offset::<InputControlContext>(1);

            // for each endpoint context designated by a Drop Context flag = 2
            for i in (2..=31).map(EndpointId::from) {
                // unwrap: only None when index not in 2..=31
                if input_ctx.drop_context_bit(i).unwrap() {
                    let mut out_ep_ctx =
                        Self::endpoint_context(out_slot_addr, i, memctx)?;
                    // set output EP State field to disabled
                    out_ep_ctx.mutate(|ctx| {
                        ctx.set_endpoint_state(EndpointState::Disabled)
                    });
                    probes::xhci_endpoint_state!(|| (
                        u8::from(slot_id),
                        u8::from(i),
                        out_ep_ctx.endpoint_state() as u8
                    ));
                    // drop endpoint from 'pipe scheduling list', in effect
                    self.slot_mut(slot_id)
                        .unwrap()
                        .unset_endpoint_transfer_ring(i);
                }
            }

            // if all input endpoint contexts with Add Context = 1 are valid
            for i in (1..=31).map(EndpointId::from) {
                if input_ctx.add_context_bit(i).unwrap() {
                    let in_ep_ctx =
                        Self::endpoint_context(in_slot_addr, i, memctx)?;
                    // not all input ep contexts valid -> Parameter Error
                    if !in_ep_ctx.valid_for_configure_endpoint() {
                        slog::warn!(
                            self.log,
                            "xHCI Configure Endpoint: Invalid context for {i:?}: {in_ep_ctx:?}"
                        );
                        return Ok(TrbCompletionCode::ParameterError);
                    }
                }
            }

            let mut any_endpoint_enabled = false;
            // for each endpoint context designated by an Add Context flag = 1
            for i in (1..=31).map(EndpointId::from) {
                let mut out_ep_ctx =
                    Self::endpoint_context(out_slot_addr, i, memctx)?;

                // unwrap: only None when index > 31
                if input_ctx.add_context_bit(i).unwrap() {
                    // copy all fields of input ep context to output ep context;
                    // set output EP state field to running.
                    let in_ep_ctx =
                        Self::endpoint_context(in_slot_addr, i, memctx)?;
                    if let Err(e) = self
                        .usbdev_for_slot(slot_id)
                        .unwrap()
                        .configure_endpoint(slot_id, i, &in_ep_ctx)
                    {
                        // just warn if nonexistent endpoint_id given -
                        // there seems to be no provision in 4.6.6 for xHC to
                        // know/care, and the subsequent USB SET_CONFIGURATION
                        // sent by guest will error accordingly
                        slog::warn!(
                            self.log,
                            "xHCI Configure Endpoint Command in {slot_id:?}: {e}"
                        );
                    }
                    out_ep_ctx.mutate(|ctx| {
                        *ctx = *in_ep_ctx;
                        ctx.set_endpoint_state(EndpointState::Running);
                    });
                    probes::xhci_endpoint_state!(|| (
                        u8::from(slot_id),
                        u8::from(i),
                        out_ep_ctx.endpoint_state() as u8
                    ));
                    // load the xHC enqueue and dequeue pointers with the
                    // value of TR Dequeue Pointer field from Endpoint Context
                    let device_slot = self.slot_mut(slot_id).unwrap();
                    device_slot.set_endpoint_transfer_ring(i, *out_ep_ctx);
                }

                if out_ep_ctx.endpoint_state() != EndpointState::Disabled {
                    any_endpoint_enabled = true;
                }
            }

            // if all Endpoints are Disabled
            if !any_endpoint_enabled {
                out_slot_ctx.mutate(|ctx| {
                    // set Slot State in Output Slot Context to Addressed
                    ctx.set_slot_state(SlotState::Addressed);
                    // set Context Entries in Output Slot Context to 1
                    ctx.set_context_entries(1);
                });
            } else {
                out_slot_ctx.mutate(|ctx| {
                    // set Slot State in Output Slot Context to Configured
                    ctx.set_slot_state(SlotState::Configured);
                    // set Context Entries to the index of the last valid
                    // Endpoint Context in the Output Device Context
                });
            }
        }

        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.7: Evaluate Context Command.
    /// (see also sections 6.2.2.3 and 6.2.3.3)
    ///
    /// Used to inform the xHC of certain Device Context fields whose values
    /// aren't known before the slot is enabled and the device is addressed,
    /// such as the Max Packet Size, which is determined after retrieving the
    /// USB Device Descriptor.
    pub fn evaluate_context(
        &self,
        slot_id: SlotId,
        input_context_ptr: GuestAddr,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }

        let input_ctx = memctx
            .read::<InputControlContext>(input_context_ptr)
            .ok_or(Error::MemCtx)?;

        // retrieve the output device context of the selected device slot
        let out_slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let mut out_slot_ctx =
            MemCtxValue::<SlotContext>::new(out_slot_addr, memctx)?;

        // only if the output slot state is default, addressed, or configured,
        if !matches!(
            out_slot_ctx.slot_state(),
            SlotState::Default | SlotState::Addressed | SlotState::Configured,
        ) {
            slog::error!(
                self.log,
                "Evaluate Context for slot in state {:?}",
                out_slot_ctx.slot_state()
            );
            return Ok(TrbCompletionCode::ContextStateError);
        }
        slog::trace!(
            self.log,
            "input_ctx: {:#x} {input_ctx:?}",
            input_context_ptr.0
        );

        let in_slot_addr = input_context_ptr.offset::<InputControlContext>(1);

        // for each context designated by an add context flag = 1,
        // evaluate the parameter settings defined by the selected contexts.
        // (limited to context indeces 0 and 1, per xHCI 1.2 sect 6.2.3.3)

        // xHCI 1.2 sect 6.2.2.3: interrupter target & max exit latency
        if input_ctx.add_context_bit(EndpointId(0)).unwrap() {
            let in_slot_ctx = memctx
                .read::<SlotContext>(in_slot_addr)
                .ok_or(Error::MemCtx)?;
            out_slot_ctx.mutate(|ctx| {
                ctx.set_interrupter_target(in_slot_ctx.interrupter_target());
                ctx.set_max_exit_latency_micros(
                    in_slot_ctx.max_exit_latency_micros(),
                );
            });
            slog::trace!(
                self.log,
                "out_slot_ctx: in@{:#x} out@{:#x} {out_slot_ctx:?}",
                in_slot_addr.0,
                out_slot_addr.0
            );
        }
        // xHCI 1.2 sect 6.2.3.3: pay attention to max packet size
        const EP_1: EndpointId = EndpointId(1);
        if input_ctx.add_context_bit(EP_1).unwrap() {
            let in_ep0_addr = input_context_ptr
                .offset::<InputControlContext>(1)
                .offset::<SlotContext>(1);
            let out_ep0_addr = out_slot_addr.offset::<SlotContext>(1);

            let in_ep0_ctx = memctx
                .read::<EndpointContext>(in_ep0_addr)
                .ok_or(Error::MemCtx)?;

            let mut out_ep0_ctx =
                Self::endpoint_context(out_slot_addr, EP_1, memctx)?;
            out_ep0_ctx.mutate(|ctx| {
                ctx.set_max_packet_size(in_ep0_ctx.max_packet_size())
            });

            slog::trace!(
                self.log,
                "out_slot_ctx: {:#x} {out_slot_ctx:?}",
                out_slot_addr.0
            );
            slog::trace!(
                self.log,
                "out_ep0_ctx: in@{:#x} out@{:#x} {out_ep0_ctx:?}",
                in_ep0_addr.0,
                out_ep0_addr.0
            );
        }
        // (again, per xHCI 1.2 sect 6.2.3.3: contexts 2 through 31
        // are not evaluated by this command.)
        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.8: Reset Endpoint Command.
    ///
    /// Issued by software when an endpoint is in [EndpointState::Halted] to
    /// recover and transition to [EndpointState::Stopped].
    /// (Note: this nominally can come about when an error occurs in e.g. TRB
    /// processing, but at time of writing our xHC has no paths that put an
    /// endpoint into [EndpointState::Halted])
    pub fn reset_endpoint(
        &mut self,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        transfer_state_preserve: bool,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        let slot_addr = self.dev_context_addr(slot_id, memctx)?;

        let mut ep_ctx =
            Self::endpoint_context(slot_addr, endpoint_id, memctx)?;

        // endpoint must be in halted state for reset endpoint command
        if ep_ctx.endpoint_state() != EndpointState::Halted {
            return Ok(TrbCompletionCode::ContextStateError);
        }
        if transfer_state_preserve {
            // when TSP=1, we wish to retry last transaction the next time
            // the doorbell is rung, if no other commands have been issued
            // to the endpoint
            let dev = match self.usbdev_for_slot(slot_id) {
                Ok(x) => x,
                Err(e) => {
                    slog::error!(self.log, "{e}");
                    return Ok(TrbCompletionCode::ContextStateError);
                }
            };
            let trb_opt = match dev.stop_transfers_on_endpoint(endpoint_id) {
                Ok(x) => x,
                Err(e) => {
                    slog::error!(
                        self.log,
                        "Error stopping {slot_id:?} {endpoint_id:?}: {e}"
                    );
                    return Ok(TrbCompletionCode::ContextStateError);
                }
            };
            if let Some(trb) = trb_opt {
                let slot = self.slot_mut(slot_id).unwrap();
                if let Err(e) = Self::write_trdp_and_ccs(
                    slot.transfer_ring_mut(&endpoint_id)?,
                    &mut ep_ctx,
                    trb.trb_pointer(),
                    trb.cycle_state(),
                ) {
                    slog::error!(
                        self.log,
                        "Error in {slot_id:?} {endpoint_id:?}: {e}"
                    );
                    return Ok(TrbCompletionCode::ContextStateError);
                }
            }
        } else {
            // protocol steps not relevant to current implementation:
            // - reset data toggle for usb2 device / sequence number for usb3 device
            // - reset any usb2 split transaction state on this endpoint

            // invalidate all cached Transfer TRBs
            match self.usbdev_for_slot(slot_id) {
                Ok(dev) => {
                    if let Err(e) = dev.stop_transfers_on_endpoint(endpoint_id)
                    {
                        slog::error!(
                            self.log,
                            "Error stopping {slot_id:?} {endpoint_id:?}: {e}"
                        );
                        return Ok(TrbCompletionCode::ContextStateError);
                    }
                }
                Err(e) => {
                    slog::error!(self.log, "{e}");
                    return Ok(TrbCompletionCode::ContextStateError);
                }
            }
        }
        // set the endpoint context ep state to stopped
        ep_ctx.mutate(|ctx| ctx.set_endpoint_state(EndpointState::Stopped));
        probes::xhci_endpoint_state!(|| (
            u8::from(slot_id),
            u8::from(endpoint_id),
            ep_ctx.endpoint_state() as u8
        ));
        // enable the doorbell register for the pipe
        // (and 'after the command completes, the Transfer Ring will be
        // reinstated on the xHC's Pipe Schedule')
        let slot = self.slot_mut(slot_id)?;
        slot.unset_endpoint_transfer_ring(endpoint_id);
        slot.set_endpoint_transfer_ring(endpoint_id, *ep_ctx);

        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.9: Stop Endpoint Command.
    ///
    /// Issued by software to stop execution of [Transfer Descriptors] on the
    /// given [EndpointId], such as to allow the guest's xHCD to modify,
    /// reorder, or abort TDs on the ring that are yet to execute.
    ///
    /// [Transfer Descriptors]: crate::hw::usb::xhci::rings::consumer::transfer
    pub fn stop_endpoint(
        &mut self,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        _suspend: bool,
        memctx: &MemCtx,
        event_sender: &EventSender,
    ) -> Result<TrbCompletionCode> {
        // only if enabled by previous enable slot command
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }
        // retrieve dev ctx
        let slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let output_slot_ctx =
            memctx.read::<SlotContext>(slot_addr).ok_or(Error::MemCtx)?;

        if !matches!(
            output_slot_ctx.slot_state(),
            SlotState::Default | SlotState::Addressed | SlotState::Configured
        ) {
            slog::error!(
                self.log,
                "Stop Endpoint for slot in state {:?}",
                output_slot_ctx.slot_state()
            );
            return Ok(TrbCompletionCode::ContextStateError);
        }

        let mut trdp;
        let mut ccs;
        {
            // unwrap: !self.slot(slot_id).is_err(), above
            let slot = self.slot_mut(slot_id).unwrap();
            let xfer_ring =
                slot.endpoints.get_mut(&endpoint_id).ok_or_else(|| {
                    Error::EndpointNotPresent(slot_id, endpoint_id)
                })?;
            trdp = xfer_ring.current_dequeue_pointer();
            ccs = xfer_ring.consumer_cycle_state();
        }
        let mut ep_ctx =
            Self::endpoint_context(slot_addr, endpoint_id, memctx)?;
        if ep_ctx.endpoint_state() != EndpointState::Running {
            slog::error!(
                self.log,
                "Stop Endpoint for endpoint in state {:?}",
                ep_ctx.endpoint_state(),
            );
            return Ok(TrbCompletionCode::ContextStateError);
        }
        if let Ok(dev) = self.usbdev_for_slot(slot_id) {
            // stop USB activity for pipe
            // stop transfer ring activity for pipe
            // remove endpoint from pipe schedule
            if let Ok(Some(trb)) = dev.stop_transfers_on_endpoint(endpoint_id) {
                trdp = trb.trb_pointer();
                ccs = trb.cycle_state();
                // if we interrupted the execution of a TD, insert a transfer event
                if let Err(e) = event_sender.enqueue_event(
                    EventInfo::Transfer {
                        trb_pointer: trb.trb_pointer(),
                        completion_code: TrbCompletionCode::Stopped,
                        trb_transfer_length: trb.data_buffer().len() as u32,
                        slot_id,
                        endpoint_id,
                        // Event Data TRBs are rolled up into the TransferTrb
                        // struct and processed at the same time as the TRB
                        // they immediately follow, so we will always be
                        // pointing to a non-Event-Data TRB in this case
                        event_data: false,
                    },
                    // will interrupt after enqueueing the command completion event
                    true,
                ) {
                    slog::error!(
                        self.log,
                        "Failed to insert event for interrupted TD in {slot_id:?} {endpoint_id:?}: {e}"
                    );
                }
            }
        }

        // unwrap: !self.slot(slot_id).is_err(), above
        let slot = self.slot_mut(slot_id).unwrap();
        let xfer_ring = slot.transfer_ring_mut(&endpoint_id)?;

        if let Err(e) =
            Self::write_trdp_and_ccs(xfer_ring, &mut ep_ctx, trdp, ccs)
        {
            slog::error!(self.log, "Error in {slot_id:?} {endpoint_id:?}: {e}");
            return Ok(TrbCompletionCode::ContextStateError);
        }

        // set ep state to stopped
        ep_ctx.mutate(|ctx| ctx.set_endpoint_state(EndpointState::Stopped));
        probes::xhci_endpoint_state!(|| (
            u8::from(slot_id),
            u8::from(endpoint_id),
            ep_ctx.endpoint_state() as u8
        ));

        // n/a: wait for any partially completed split transactions
        Ok(TrbCompletionCode::Success)
    }

    /// xHCI 1.2 sect 4.6.10: Set TR Dequeue Pointer Command.
    ///
    /// Issued by software to modify the TRDP of an Endpoint or Stream Context.
    /// Only may execute if the given endpoint is in [EndpointState::Error] or
    /// [EndpointState::Stopped].
    pub fn set_tr_dequeue_pointer(
        &mut self,
        new_tr_dequeue_ptr: GuestAddr,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        dequeue_cycle_state: bool,
        memctx: &MemCtx,
        event_sender: &EventSender,
    ) -> Result<TrbCompletionCode> {
        // only if enabled by previous enable slot command
        if self.slot(slot_id).is_err() {
            return Ok(TrbCompletionCode::SlotNotEnabledError);
        }

        let slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let output_slot_ctx =
            memctx.read::<SlotContext>(slot_addr).ok_or(Error::MemCtx)?;

        // only if slot state is default, configured, or addressed
        if !matches!(
            output_slot_ctx.slot_state(),
            SlotState::Default | SlotState::Addressed | SlotState::Configured
        ) {
            return Ok(TrbCompletionCode::ContextStateError);
        }

        let mut ep_ctx =
            Self::endpoint_context(slot_addr, endpoint_id, memctx)?;
        // only if endpoint's state is stopped or error
        if !matches!(
            ep_ctx.endpoint_state(),
            EndpointState::Stopped | EndpointState::Error
        ) {
            return Ok(TrbCompletionCode::ContextStateError);
        }

        // TODO(USB3): behavior is different for stream-enabled endpoints
        // (TRDP refers to Stream Context array rather than a Transfer Ring);
        // command trb decode currently assumes MaxPStreams and StreamID are 0

        // invalidate any cached TDs, forcing xHC to re-retrieve them when
        // endpoint transitions back into Running state
        if let Ok(dev) = self.usbdev_for_slot(slot_id) {
            if let Err(e) = dev.stop_transfers_on_endpoint(endpoint_id) {
                // xHCI 4.10.2: "All Transfer Ring error conditions force the
                // state of the associated endpoint to Halted and require system
                // software intervention to recover."
                ep_ctx.mutate(|ctx| {
                    ctx.set_endpoint_state(EndpointState::Halted)
                });
                slog::error!(
                    self.log,
                    "Failed to stop endpoint {slot_id:?} {endpoint_id:?}: {e}"
                );
            }
        }

        // unwrap: !self.slot(slot_id).is_err(), above
        let slot = self.slot_mut(slot_id).unwrap();
        let xfer_ring = slot.transfer_ring_mut(&endpoint_id)?;

        // copy new_tr_dequeue_ptr to target Endpoint Context
        // copy dequeue_cycle_state to target Endpoint Context
        if let Err(e) = Self::write_trdp_and_ccs(
            xfer_ring,
            &mut ep_ctx,
            new_tr_dequeue_ptr,
            dequeue_cycle_state,
        ) {
            slog::error!(self.log, "Error on {slot_id:?} {endpoint_id:?}: {e}");
            return Ok(TrbCompletionCode::ContextStateError);
        }

        // clear prior transfer state, e.g. setting EDTLA to 0
        event_sender.reset_edtla();

        Ok(TrbCompletionCode::Success)
    }

    fn write_trdp_and_ccs(
        xfer_ring: &mut TransferRing,
        ep_ctx: &mut MemCtxValue<'_, EndpointContext>,
        new_tr_dequeue_ptr: GuestAddr,
        dequeue_cycle_state: bool,
    ) -> Result<()> {
        ep_ctx.mutate(|ctx| {
            ctx.set_tr_dequeue_pointer(new_tr_dequeue_ptr);
            ctx.set_dequeue_cycle_state(dequeue_cycle_state)
        });

        xfer_ring
            .set_dequeue_pointer_and_cycle(
                new_tr_dequeue_ptr,
                dequeue_cycle_state,
            )
            .map_err(|e| {
                Error::SetTRDPFailed(new_tr_dequeue_ptr, dequeue_cycle_state, e)
            })
    }

    /// xHCI 1.2 sect 4.6.11: Reset Device Command.
    ///
    /// Issued by software to inform xHC that the device in the given [SlotId]
    /// has been reset (such as by writing the associated root hub port's
    /// [PortStatusControl::port_reset] flag.
    /// Sets the slot to [SlotState::Default], the USB device address to 0,
    /// and all endpoints besides the DCE to [EndpointState::Disabled].
    pub fn reset_device(
        &mut self,
        slot_id: SlotId,
        memctx: &MemCtx,
    ) -> Result<TrbCompletionCode> {
        let slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let mut output_slot_ctx =
            MemCtxValue::<SlotContext>::new(slot_addr, memctx)?;
        if !matches!(
            output_slot_ctx.slot_state(),
            SlotState::Addressed | SlotState::Configured
        ) {
            return Ok(TrbCompletionCode::ContextStateError);
        }
        let last_endpoint = output_slot_ctx.context_entries();

        // abort any usb transactions to the device
        let log = self.log.clone();
        if let Ok(dev) = self.usbdev_for_slot(slot_id) {
            for endpoint_id in (1..last_endpoint).map(EndpointId::from) {
                if let Err(e) = dev.stop_transfers_on_endpoint(endpoint_id) {
                    slog::error!(
                        log,
                        "Failed to abort {endpoint_id:?} while resetting device in {slot_id:?}: {e}"
                    );
                }
            }
        }

        // set slot state to default
        // set context entries to 1
        // set usb dev address to 0
        output_slot_ctx.mutate(|ctx| {
            ctx.set_slot_state(SlotState::Default);
            ctx.set_context_entries(1);
            ctx.set_usb_device_address(0);
        });

        // xHCI 1.2 sect 6.2.3.7: Default Control Endpoint's
        // Max Packet Size, EP Type, CErr, TR Dequeue Pointer, and
        // Average TRB Length are unchanged by Reset Device Command,
        // the EP State field shall be set to the Running state, and
        // all other fields shall be set to 0.
        let mut ep0_ctx =
            Self::endpoint_context(slot_addr, EndpointId::from(1), memctx)?;
        ep0_ctx.mutate(|ctx| {
            ctx.set_mult(0);
            ctx.set_max_primary_streams(0);
            ctx.set_linear_stream_array(false);
            ctx.set_interval(0);
            ctx.set_max_endpoint_service_time_interval_payload_high(0);
            ctx.set_host_initiate_disable(false);
            ctx.set_max_burst_size(0);
            ctx.set_endpoint_state(EndpointState::Running)
        });
        probes::xhci_endpoint_state!(|| (
            u8::from(slot_id),
            1u8,
            ep0_ctx.endpoint_state() as u8
        ));

        // for each endpoint context besides the Default Control Endpoint
        for endpoint_id in (2..=last_endpoint).map(EndpointId::from) {
            let mut ep_ctx =
                Self::endpoint_context(slot_addr, endpoint_id, memctx)?;
            // set ep state to disabled
            ep_ctx
                .mutate(|ctx| ctx.set_endpoint_state(EndpointState::Disabled));
            probes::xhci_endpoint_state!(|| (
                u8::from(slot_id),
                u8::from(endpoint_id),
                ep_ctx.endpoint_state() as u8
            ));
        }

        Ok(TrbCompletionCode::Success)
    }

    /// Return a reference to the [TransferRing] associated with the given
    /// [EndpointId], provided it has been appropriately enabled previously,
    /// on the occasion of a write to the given [SlotId]'s doorbell register.
    pub fn transfer_ring_for_doorbell(
        &mut self,
        slot_id: SlotId,
        endpoint_id: EndpointId,
        memctx: &MemCtx,
    ) -> Result<&mut TransferRing> {
        let log = self.log.clone();

        let slot_addr = self.dev_context_addr(slot_id, memctx)?;
        let slot = match self.slot_mut(slot_id) {
            Ok(slot) => slot,
            Err(e) => {
                slog::error!(log, "rang Doorbell for non-enabled {slot_id:?}");
                return Err(e);
            }
        };
        let slot_ctx =
            memctx.read::<SlotContext>(slot_addr).ok_or(Error::MemCtx)?;
        if slot_ctx.slot_state() == SlotState::DisabledEnabled {
            // xHCI 1.2 sect 4.7: "An Endpoint Not Enabled Error *should* be
            // generated for doorbell register writes to Device Slots that
            // are in the Disabled state regardless of the DB Target value
            // provided."
            // (emphasis mine on 'should'.)
            return Err(Error::SlotNotEnabled(slot_id));
        }
        let endpoint = slot.transfer_ring_mut(&endpoint_id)?;

        // xHCI 1.2 figure 4-5: transition state to Running from Stopped
        let mut ep_ctx =
            Self::endpoint_context(slot_addr, endpoint_id, memctx)?;
        match ep_ctx.endpoint_state() {
            EndpointState::Running => Ok(endpoint),
            EndpointState::Stopped => {
                ep_ctx.mutate(|ctx| {
                    ctx.set_endpoint_state(EndpointState::Running)
                });
                probes::xhci_endpoint_state!(|| (
                    u8::from(slot_id),
                    u8::from(endpoint_id),
                    ep_ctx.endpoint_state() as u8
                ));
                Ok(endpoint)
            }
            EndpointState::Disabled
            | EndpointState::Halted
            | EndpointState::Error => Err(Error::EndpointInappropriateState(
                slot_id,
                endpoint_id,
                ep_ctx.endpoint_state(),
            )),
            EndpointState::Reserved5
            | EndpointState::Reserved6
            | EndpointState::Reserved7 => {
                Err(Error::EndpointInvalidState(slot_id, endpoint_id))
            }
        }
    }

    pub fn export(
        &self,
    ) -> core::result::Result<
        migrate::DeviceSlotTableV1,
        crate::migrate::MigrateStateError,
    > {
        let Self { dcbaap, slots, config_max_slots: _, port_devs, log: _ } =
            self;
        Ok(migrate::DeviceSlotTableV1 {
            dcbaap: dcbaap.as_ref().map(|x| x.0),
            slots: slots
                .iter()
                .map(|opt| opt.as_ref().map(|slot| slot.export()))
                .collect(),
            port_devs: port_devs
                .iter()
                .map(|opt| {
                    opt.as_ref().map(|usbdev| usbdev.export()).transpose()
                })
                .collect::<core::result::Result<Vec<_>, _>>()?,
        })
    }

    pub fn import(
        &mut self,
        value: &migrate::DeviceSlotTableV1,
        ctx: &crate::migrate::MigrateCtx,
        port_handles: &super::controller::XhciPortHandles,
        pci_state: &Arc<pci::DeviceState>,
        guest_time: &VmGuestTime,
        config_max_slots: u8,
    ) -> core::result::Result<(), crate::migrate::MigrateStateError> {
        let migrate::DeviceSlotTableV1 { dcbaap, slots, port_devs } = value;
        self.dcbaap = dcbaap.map(GuestAddr);
        if port_devs.len() != self.port_devs.len() {
            return Err(crate::migrate::MigrateStateError::ImportFailed(
                format!(
                    "payload port devs array size wrong: {} != {}",
                    port_devs.len(),
                    self.port_devs.len()
                ),
            ));
        }

        // zip: port_devs is of fixed size
        for (port_index_raw, (dst, src)) in
            self.port_devs.iter_mut().zip(port_devs).enumerate()
        {
            if src.is_none() {
                *dst = None;
            } else {
                let src_dev = src.as_ref().unwrap();
                if let Some(dst_dev) = dst {
                    // device exists in this port already, update its state
                    dst_dev.import(src_dev)?;
                } else {
                    let port_id =
                        PortId::try_from((port_index_raw + 1) as u8).unwrap();
                    let dst_dev = UsbDeviceType::create_from_payload(
                        src_dev,
                        ctx.hid_report,
                        port_handles.handle_for_port(port_id),
                        pci_state,
                        guest_time,
                        &self.log,
                    )?;
                    *dst = Some(dst_dev);
                }
            }
        }

        // zip with separate iterator: incoming payload may have more slots,
        // self.slots is dynamically sized
        let mut payload_slots_iter = slots.into_iter();
        for (i, (dst, src)) in
            self.slots.iter_mut().zip(&mut payload_slots_iter).enumerate()
        {
            if let Some(src_slot) = src {
                if let Some(dst_slot) = dst {
                    dst_slot.import(src_slot)?;
                } else {
                    // unwrap: normal logic added a slot to this index already
                    let slot_id = SlotId::from_index(i).unwrap();
                    let mut dst_slot = DeviceSlot::new(slot_id);
                    dst_slot.import(src_slot)?;
                    *dst = Some(dst_slot);
                }
            } else {
                *dst = None;
            }
        }
        // as for the rest of the iterator that wasn't consumed in the zip:
        for src in payload_slots_iter {
            if let Some(src_slot) = src {
                let slot_id =
                    SlotId::from_index(self.slots.len()).map_err(|e| {
                        crate::migrate::MigrateStateError::ImportFailed(
                            e.to_string(),
                        )
                    })?;
                let mut dst_slot = DeviceSlot::new(slot_id);
                dst_slot.import(src_slot)?;
                self.slots.push(Some(dst_slot));
            } else {
                self.slots.push(None);
            }
        }

        self.config_max_slots = config_max_slots;

        Ok(())
    }
}

pub mod migrate {
    use crate::hw::usb::usbdev::migrate::UsbDeviceV1;
    use crate::hw::usb::xhci::rings::consumer::migrate::ConsumerRingV1;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    #[derive(Serialize, Deserialize)]
    pub struct DeviceSlotV1 {
        pub endpoints: HashMap<u8, ConsumerRingV1>,
        pub port_address: Option<u8>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct DeviceSlotTableV1 {
        pub dcbaap: Option<u64>,
        pub slots: Vec<Option<DeviceSlotV1>>,
        pub port_devs: Vec<Option<UsbDeviceV1>>,
    }
}

#[cfg(test)]
mod test {
    use zerocopy::FromZeros;

    use super::*;
    use crate::hw::usb::test::pci_state_for_test;
    use crate::hw::usb::usbdev::demo_state_tracker::NullUsbDevice;
    use crate::hw::usb::xhci::bits::device_context::EndpointType;
    use crate::hw::usb::xhci::controller::XhciPortHandle;

    /// Tests each of the implemented Command TRBs according to the xHC
    /// behaviors specified in the xHCI specification (most significantly the
    /// descriptions in section 4.6, but with occasional additional
    /// requirements overlooked in that section but described elsewhere
    /// in the document).
    #[test]
    fn device_and_endpoint_management() {
        const DCBAAP: GuestAddr = GuestAddr(1024);
        // a device context is max 1KiB (32 contexts, each 32 bytes)
        const DC_ADDR: GuestAddr = GuestAddr(2 * 1024);
        const IN_DC_ADDR: GuestAddr = GuestAddr(3 * 1024);
        const TRDP: GuestAddr = GuestAddr(4 * 1024);

        let port_id = PortId::try_from(1).unwrap();
        let ep_id_1 = EndpointId::from(1);
        let ep_id_3 = EndpointId::from(3);

        let log = slog::Logger::root(slog::Discard, slog::o!());
        let pci_state = pci_state_for_test();
        let event_sender = Arc::new(EventSender::new(&pci_state));
        let port_hdl = Arc::new(XhciPortHandle::new_test(event_sender.clone()));
        let memctx = pci_state.acc_mem.access().unwrap();

        let mut table = DeviceSlotTable::new(log.clone());
        table.set_dcbaap(DCBAAP);

        let usb_dev = Box::new(NullUsbDevice::new(port_hdl, log.clone()));

        // enabling port
        if table.attach_to_root_hub_port_address(port_id, usb_dev).is_some() {
            unreachable!("new dev table is not already occupied")
        };
        // (pretend xHC sends guest a Port Status Change Event here)

        // prior to any slots being enabled, Address Device is invalid
        assert_eq!(
            table
                .address_device(SlotId::from(1), IN_DC_ADDR, true, &memctx)
                .unwrap(),
            TrbCompletionCode::SlotNotEnabledError
        );

        // * Enable Slot Command
        // should fail if we haven't raised the max slot ceiling in CONFIG
        assert!(table.enable_slot(0).is_none());
        // but after that...
        table.set_max_slots(1);
        // 'slot type' 0 is USB
        let slot_id = table.enable_slot(0).unwrap();

        // create context structs (xHCI 1.2 fig 4-3)
        let mut in_control_ctx = InputControlContext::new_zeroed();
        let mut in_slot_ctx = SlotContext::new_zeroed();
        in_slot_ctx.set_slot_state(SlotState::DisabledEnabled);
        in_slot_ctx.set_root_hub_port_number(port_id);
        in_slot_ctx.set_context_entries(3);
        let mut in_ep0_ctx = EndpointContext::new_zeroed();
        in_ep0_ctx.set_endpoint_state(EndpointState::Disabled);
        in_ep0_ctx.set_endpoint_type(EndpointType::Control);
        in_ep0_ctx.set_tr_dequeue_pointer(TRDP);
        in_ep0_ctx.set_dequeue_cycle_state(true);
        let mut in_intr_in_ctx = EndpointContext::new_zeroed();
        in_intr_in_ctx.set_endpoint_state(EndpointState::Disabled);
        in_intr_in_ctx.set_endpoint_type(EndpointType::InterruptIn);
        in_intr_in_ctx.set_tr_dequeue_pointer(TRDP);
        in_intr_in_ctx.set_dequeue_cycle_state(true);

        assert!(memctx.write(IN_DC_ADDR, &in_control_ctx));
        assert!(memctx
            .write(IN_DC_ADDR.offset::<InputControlContext>(1), &in_slot_ctx));
        assert!(memctx.write(IN_DC_ADDR.offset::<SlotContext>(2), &in_ep0_ctx));
        assert!(
            memctx.write(IN_DC_ADDR.offset::<SlotContext>(4), &in_intr_in_ctx)
        );

        // put pointer to device context into DCBAA
        assert!(memctx.write(
            DCBAAP.offset::<GuestAddr>(u8::from(slot_id) as usize),
            &DC_ADDR,
        ));

        // * Address Device Command (BSR=1)
        assert_eq!(
            table.address_device(slot_id, IN_DC_ADDR, true, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        let out_slot_ctx = memctx.read::<SlotContext>(DC_ADDR).unwrap();
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        // xHC will have copied input device context to DC_ADDR...
        assert_eq!(out_slot_ctx.root_hub_port_number().unwrap(), port_id);
        assert_eq!(out_ep0_ctx.endpoint_type(), EndpointType::Control);
        // but also changed the slot and endpoint states
        assert_eq!(out_slot_ctx.slot_state(), SlotState::Default);
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Running);
        // and associated the transfer ring from the ep0 context
        let xfer_ring = table
            .transfer_ring_for_doorbell(slot_id, ep_id_1, &memctx)
            .unwrap();
        assert_eq!(xfer_ring.current_dequeue_pointer(), TRDP);
        assert!(xfer_ring.consumer_cycle_state());
        // but not the non-default-control-endpoint endpoint until we config it
        assert_eq!(out_intr_in_ctx.endpoint_type(), EndpointType::NotValid);
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Disabled);
        assert!(table
            .transfer_ring_for_doorbell(slot_id, ep_id_3, &memctx)
            .is_err());

        // * Address Device Command (BSR=0)
        assert_eq!(
            table.address_device(slot_id, IN_DC_ADDR, false, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        let out_slot_ctx = memctx.read::<SlotContext>(DC_ADDR).unwrap();
        assert_eq!(out_slot_ctx.slot_state(), SlotState::Addressed);

        // ep 3 not in running state yet, so attempt to stop it should fail
        assert!(table
            .stop_endpoint(slot_id, ep_id_3, false, &memctx, &event_sender)
            .is_err());

        // * Configure Endpoint Command (DeConfigure=0)
        in_control_ctx.set_add_context_bit(ep_id_3, true);
        assert!(memctx.write(IN_DC_ADDR, &in_control_ctx));
        // should fail if the enabling ep ctx doesn't pass validation rules
        assert_eq!(
            table
                .configure_endpoint(IN_DC_ADDR, slot_id, false, &memctx)
                .unwrap(),
            TrbCompletionCode::ParameterError
        );
        // let's just fix that...
        in_intr_in_ctx.set_average_trb_length(1);
        assert!(
            memctx.write(IN_DC_ADDR.offset::<SlotContext>(4), &in_intr_in_ctx)
        );
        // *now* it should complete successfully
        assert_eq!(
            table
                .configure_endpoint(IN_DC_ADDR, slot_id, false, &memctx)
                .unwrap(),
            TrbCompletionCode::Success
        );
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        // now should have been copied from input ep context
        assert_eq!(out_intr_in_ctx.endpoint_type(), EndpointType::InterruptIn);
        // DCE should still be running, and now interrupt-in EP should too
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Running);
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Running);

        // cannot set TRDP of running endpoint
        assert_eq!(
            table
                .set_tr_dequeue_pointer(
                    TRDP,
                    slot_id,
                    ep_id_3,
                    false,
                    &memctx,
                    &event_sender
                )
                .unwrap(),
            TrbCompletionCode::ContextStateError
        );

        // * Stop Endpoint Command
        assert_eq!(
            table
                .stop_endpoint(slot_id, ep_id_3, false, &memctx, &event_sender,)
                .unwrap(),
            TrbCompletionCode::Success
        );
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Stopped);

        // endpoint stopped, can now set TRDP
        table
            .set_tr_dequeue_pointer(
                TRDP,
                slot_id,
                ep_id_3,
                false,
                &memctx,
                &event_sender,
            )
            .unwrap();

        // ringing stopped endpoint's doorbell should un-stop it
        table.transfer_ring_for_doorbell(slot_id, ep_id_3, &memctx).unwrap();
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Running);

        // * Configure Endpoint Command (DeConfigure=1)
        assert_eq!(
            table
                .configure_endpoint(IN_DC_ADDR, slot_id, true, &memctx)
                .unwrap(),
            TrbCompletionCode::Success
        );
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        // DCE should still be running, but our interrupt-in EP should be disabled
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Running);
        assert_eq!(out_intr_in_ctx.endpoint_type(), EndpointType::InterruptIn);
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Disabled);

        // ringing disabled endpoint's doorbell should fail; its transfer ring
        // has been removed outright
        assert!(table
            .transfer_ring_for_doorbell(slot_id, ep_id_3, &memctx)
            .is_err());

        // * Reset Endpoint Command
        // should fail if the endpoint is not in 'Halted' state
        assert_eq!(
            table.reset_endpoint(slot_id, ep_id_1, false, &memctx).unwrap(),
            TrbCompletionCode::ContextStateError
        );

        // so let's simulate a halted condition: "When a Transfer Ring or
        // Stream is halted; the associated endpoint is removed from the xHC's
        // Pipe Schedule, the Doorbell Register for that pipe is disabled, the
        // state of the associated Endpoint Context is set to Halted, and any
        // subsequent packets received for the endpoint will be silently
        // dropped." - xHCI 1.2 sect 4.6.8
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap()
            .with_endpoint_state(EndpointState::Halted);
        assert!(memctx.write(DC_ADDR.offset::<SlotContext>(1), &out_ep0_ctx));
        table.slots[slot_id.as_index().unwrap()]
            .as_mut()
            .unwrap()
            .unset_endpoint_transfer_ring(ep_id_1);

        // *now* it's appropriate to send Reset Endpoint
        assert_eq!(
            table.reset_endpoint(slot_id, ep_id_1, false, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        // which sets the endpoint state to Stopped
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Stopped);
        // and re-enables its doorbell / reinstates the Transfer Ring
        table.transfer_ring_for_doorbell(slot_id, ep_id_1, &memctx).unwrap();

        // * Evaluate Context Command
        in_control_ctx.set_add_context_bit(EndpointId::from(0), true);
        in_control_ctx.set_add_context_bit(ep_id_1, true);
        in_slot_ctx.set_interrupter_target(2);
        in_slot_ctx.set_max_exit_latency_micros(1234);
        in_ep0_ctx.set_max_packet_size(64);
        assert!(memctx.write(IN_DC_ADDR, &in_control_ctx));
        assert!(memctx
            .write(IN_DC_ADDR.offset::<InputControlContext>(1), &in_slot_ctx));
        assert!(memctx.write(IN_DC_ADDR.offset::<SlotContext>(2), &in_ep0_ctx));
        assert_eq!(
            table.evaluate_context(slot_id, IN_DC_ADDR, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        let out_slot_ctx = memctx.read::<SlotContext>(DC_ADDR).unwrap();
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        assert_eq!(out_slot_ctx.interrupter_target(), 2);
        assert_eq!(out_slot_ctx.max_exit_latency_micros(), 1234);
        assert_eq!(out_ep0_ctx.max_packet_size(), 64);

        // * Reset Device Command
        assert_eq!(
            table.reset_device(slot_id, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        let out_slot_ctx = memctx.read::<SlotContext>(DC_ADDR).unwrap();
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        let out_intr_in_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(3))
            .unwrap();
        // transitions slot state from Addressed/Configured to Default
        assert_eq!(out_slot_ctx.slot_state(), SlotState::Default);
        // context entries set to 1, usb dev addr set to 0
        assert_eq!(out_slot_ctx.context_entries(), 1);
        assert_eq!(out_slot_ctx.usb_device_address(), 0);
        // sets every ep state besides default control endpoint to Disabled
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Running);
        assert_eq!(out_intr_in_ctx.endpoint_state(), EndpointState::Disabled);

        // * Disable Slot Command
        assert_eq!(
            table.disable_slot(slot_id, &memctx).unwrap(),
            TrbCompletionCode::Success
        );
        // should disable the default control endpoint as well
        let out_ep0_ctx = memctx
            .read::<EndpointContext>(DC_ADDR.offset::<SlotContext>(1))
            .unwrap();
        assert_eq!(out_ep0_ctx.endpoint_state(), EndpointState::Disabled);
        // and remove its transfer ring
        assert!(table
            .transfer_ring_for_doorbell(slot_id, ep_id_1, &memctx)
            .is_err());

        // and once more, Address Device is invalid now for that slot
        assert_eq!(
            table.address_device(slot_id, IN_DC_ADDR, true, &memctx).unwrap(),
            TrbCompletionCode::SlotNotEnabledError
        );
    }
}
