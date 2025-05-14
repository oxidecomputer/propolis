// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
## Emulated eXtensible Host Controller Interface (xHCI) device.

The version of the standard[^xhci-std] referenced throughout the comments in
this module is xHCI 1.2, but we do not implement the features required of a
1.1 or 1.2 compliant host controller - that is, we are only implementing a
subset of what xHCI version 1.0 requires of an xHC, as described by version 1.2
of the *specification*.

[^xhci-std]: <https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf>

At present, the only USB device supported is a USB 2.0 [NullUsbDevice] with
no actual functionality, which exists as a proof-of-concept and as a means to
show that USB [DeviceDescriptor]s are successfully communicated to the guest
in phd-tests.

[NullUsbDevice]: super::usbdev::demo_state_tracker::NullUsbDevice
[DeviceDescriptor]: super::usbdev::descriptor::DeviceDescriptor

```text
   +---------+
   | PciXhci |
   +---------+
        | has-a
  +-----------------------------+
  |          XhciState          |
  |-----------------------------|
  | PCI MMIO registers          |
  | XhciInterrupter             |
  | DeviceSlotTable             |
  | Usb2Ports + Usb3Ports       |
  | CommandRing                 |
  | newly attached USB devices  |
  +-----------------------------+
      | has-a               |
+-------------------+       | has-a
|  XhciInterrupter  |   +-----------------+
|-------------------|   | DeviceSlotTable |
| EventRing         |   |-----------------|
| MSI-X/INTxPin     |   | DeviceSlot(s)   |___+------------------+
+-------------------+   | DCBAAP          |   |    DeviceSlot    |
                        | Active USB devs |   |------------------|
                        +-----------------+   | TransferRing(s)  |
                                              +------------------+
```

### Conventions

Wherever possible, the framework represents [TRB] data through a further level
of abstraction, such as enums constructed from the raw TRB bitfields before
being passed to other parts of the system that use them, such that the behavior
of identifying [TrbType] and accessing their fields properly according to the
spec lives in a conversion function rather than strewn across implementation of
other xHC functionality.

[TRB]: bits::ring_data::Trb
[TrbType]: bits::ring_data::TrbType

The nomenclature used is generally trading the "Descriptor" suffix for "Info",
e.g. the high-level enum-variant version of an [EventDescriptor] is [EventInfo]
(which is passed to the [EventRing] to be converted into Event TRBs and written
into guest memory).

[EventRing]: rings::producer::event::EventRing
[EventInfo]: rings::producer::event::EventInfo
[EventDescriptor]: rings::producer::event::EventDescriptor

For 1-based indices defined by the spec (slot ID, port ID), we use [SlotId] and
[PortId] and their respective `.as_index()` methods to index into our internal
arrays of slots and ports, such that we aspire to categorically avoid off-by-one
errors of omission (of `- 1`). (Note that indexing into the DCBAA is *not* done
with this method, as position 0 in it is reserved by the spec for the Scratchpad
described in xHCI 1.2 section 4.20)

[SlotId]: device_slots::SlotId
[PortId]: port::PortId

### Implementation

#### [DeviceSlotTable]

When a USB device is attached to the xHC, it is enqueued in a list within
[XhciState] along with its [PortId]. The next time the xHC runs:

- it will update the corresponding [PORTSC] register and inform the guest with
  a TRB on the [EventRing], and if enabled, a hardware interrupt.
- it moves the USB device to the [DeviceSlotTable] in preparation for being
  configured and assigned a slot. When the guest xHCD rings Doorbell 0 to run
  an [EnableSlot] Command, the [DeviceSlotTable] assigns the first unused
  slot ID to it.

Hot-plugging devices live (i.e. not just attaching all devices defined by the
instance spec at boot time as is done now) is not yet implemented.

[DeviceSlotTable]: device_slots::DeviceSlotTable
[XhciState]: controller::XhciState
[PortId]: port::PortId
[PORTSC]: bits::PortStatusControl
[EnableSlot]: rings::consumer::command::CommandInfo::EnableSlot

Device-slot-related Command TRBs are handled by the [DeviceSlotTable].
The command interface methods are written as translations of the behaviors
defined in xHCI 1.2 section 4.6 to Rust, with liberties taken around redundant
[TrbCompletionCode] writes; i.e. when the outlined behavior from the spec
describes the xHC placing a [Success] into a new TRB on the
[EventRing] immediately at the beginning of the command's execution, and then
overwriting it with a failure code in the event of a failure,
our implementation postpones the creation and enqueueing of the event until
after the outcome of the command's execution (and thus the Event TRB's values)
are all known.

[TrbCompletionCode]: bits::ring_data::TrbCompletionCode
[Success]: bits::ring_data::TrbCompletionCode::Success

#### Ports

Root hub port state machines (xHCI 1.2 section 4.19.1) and port registers are
managed by [Usb2Port], which has separate methods for handling register writes
by the guest and by the xHC itself.

[Usb2Port]: port::Usb2Port

#### TRB Rings

##### Consumer

The [CommandRing] and each slot endpoint's [TransferRing] are implemented as
[ConsumerRing]<[CommandInfo]> and [ConsumerRing]<[TransferInfo]>.
Dequeued work items are converted from raw [CommandDescriptor]s and
[TransferDescriptor]s, respectively.

Starting at the dequeue pointer provided by the guest, the [ConsumerRing] will
consume non-Link TRBs (and follow Link TRBs, as in xHCI 1.2 figure 4-15) into
complete work items. In the case of the [CommandRing], [CommandDescriptor]s are
each only made up of one [TRB], but for the [TransferRing] multi-TRB work items
are possible, where all but the last item have the [chain_bit] set.

[ConsumerRing]: rings::consumer::ConsumerRing
[CommandRing]: rings::consumer::command::CommandRing
[CommandInfo]: rings::consumer::command::CommandInfo
[CommandDescriptor]: rings::consumer::command::CommandDescriptor
[TransferRing]: rings::consumer::transfer::TransferRing
[TransferInfo]: rings::consumer::transfer::TransferInfo
[TransferDescriptor]: rings::consumer::transfer::TransferDescriptor
[chain_bit]: bits::ring_data::TrbControlFieldNormal::chain_bit

##### Producer

The only type of producer ring is the [EventRing]. Events destined for it are
fed through the [XhciInterrupter], which handles enablement and rate-limiting
of PCI-level machine interrupts being generated as a result of the events.

Similarly (and inversely) to the consumer rings, the [EventRing] converts the
[EventInfo]s enqueued in it into [EventDescriptor]s to be written into guest
memory regions defined by the [EventRingSegment] Table.

[XhciInterrupter]: interrupter::XhciInterrupter
[EventRingSegment]: bits::ring_data::EventRingSegment

#### Doorbells

The guest writing to a [DoorbellRegister] makes the host controller process a
consumer TRB ring (the [CommandRing] for doorbell 0, or the corresponding
slot's [TransferRing] for nonzero doorbells). The ring consumption is performed
by the doorbell register write handler, in [process_command_ring] and
[process_transfer_ring].

[DoorbellRegister]: bits::DoorbellRegister
[process_command_ring]: rings::consumer::doorbell::process_command_ring
[process_transfer_ring]: rings::consumer::doorbell::process_transfer_ring

#### Timer registers

The value of registers defined as incrementing/decrementing per time interval,
such as [MFINDEX] and the [XhciInterrupter]'s [IMODC], are simulated with
[VmGuestInstant]s and [Duration]s rather than by repeated incrementation.

[IMODC]: bits::InterrupterModeration::counter
[MFINDEX]: bits::MicroframeIndex
[VmGuestInstant]: crate::vmm::time::VmGuestInstant
[Duration]: std::time::Duration

### DTrace

To see a trace of all MMIO register reads/writes and TRB enqueue/dequeues:

```sh
pfexec ./scripts/xhci-trace.d -p $(pgrep propolis-server)
```

The name of each register as used by DTrace is `&'static`ally defined in
[registers::Registers::reg_name].

*/

// pub for rustdoc's sake
pub mod bits;
pub mod controller;
pub mod device_slots;
pub mod interrupter;
pub mod port;
pub mod registers;
pub mod rings;

pub use controller::PciXhci;

/// The number of USB2 ports the controller supports.
const NUM_USB2_PORTS: u8 = 4;

/// The number of USB3 ports the controller supports.
const NUM_USB3_PORTS: u8 = 4;

/// Value returned for HCSPARAMS1 max ports field.
const MAX_PORTS: u8 = NUM_USB2_PORTS + NUM_USB3_PORTS;

/// Max number of device slots the controller supports.
// (up to 255)
const MAX_DEVICE_SLOTS: u8 = 64;

/// Max number of interrupters the controller supports (up to 1024).
const NUM_INTRS: u16 = 1;

/// An indirection used in [PciXhci::reg_read] and [PciXhci::reg_write],
/// for reporting values to [usdt] probes.
#[derive(Copy, Clone, Debug)]
enum RegRWOpValue {
    NoOp,
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Fill(u8),
}

impl RegRWOpValue {
    fn as_u64(&self) -> u64 {
        match self {
            RegRWOpValue::NoOp => 0,
            RegRWOpValue::U8(x) => *x as u64,
            RegRWOpValue::U16(x) => *x as u64,
            RegRWOpValue::U32(x) => *x as u64,
            RegRWOpValue::U64(x) => *x,
            RegRWOpValue::Fill(x) => *x as u64,
        }
    }
}
