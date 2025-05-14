// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::{GuestAddr, GuestData};
use crate::hw::usb::xhci::bits::ring_data::*;
use crate::hw::usb::xhci::device_slots::SlotId;
use crate::hw::usb::xhci::port::PortId;
use crate::vmm::MemCtx;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_producer_ring_enqueue_trb(offset: usize, data: u64, trb_type: u8) {}
    fn xhci_producer_ring_set_dequeue_ptr(ptr: usize) {}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Event Ring full when trying to enqueue {0:?}")]
    EventRingFull(Trb),
    #[error("Tried to enqueue {0:?} in Event Ring with empty Segment Table")]
    EventRingSegmentTableSizeZero(Trb),
    #[error("Event Ring Segment Table of size {1} cannot be read from address {0:?}")]
    EventRingSegmentTableLocationInvalid(GuestAddr, usize),
    #[error("Event Ring Segment Table Entry has invalid size: {0:?}")]
    InvalidEventRingSegmentSize(EventRingSegment),
    #[error("Interrupter error")]
    Interrupter,
}
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub struct EventRing {
    /// EREP.
    enqueue_pointer: Option<GuestAddr>,

    /// xHCI 1.2 sect 4.9.4: software writes the ERDP register to inform
    /// the xHC it has completed processing TRBs up to and including the
    /// TRB pointed to by ERDP.
    dequeue_pointer: Option<GuestAddr>,

    /// ESRTE's.
    segment_table: Vec<EventRingSegment>,
    /// "ESRT Count".
    segment_table_index: usize,

    /// "TRB Count".
    segment_remaining_trbs: usize,

    /// PCS.
    producer_cycle_state: bool,
}

impl EventRing {
    pub fn new(
        erstba: GuestAddr,
        erstsz: usize,
        erdp: GuestAddr,
        memctx: &MemCtx,
    ) -> Result<Self> {
        let mut x = Self {
            enqueue_pointer: None,
            dequeue_pointer: Some(erdp),
            segment_table: Vec::new(),
            segment_table_index: 0,
            segment_remaining_trbs: 0,
            producer_cycle_state: true,
        };
        x.update_segment_table(erstba, erstsz, memctx)?;
        Ok(x)
    }

    /// Cache entire segment table. To be called when location (ERSTBA) or
    /// size (ERSTSZ) registers are written, or when host controller is resumed.
    /// (Per xHCI 1.2 sect 4.9.4.1: ERST entries themselves are not allowed
    /// to be modified by software when HCHalted = 0)
    pub fn update_segment_table(
        &mut self,
        erstba: GuestAddr,
        erstsz: usize,
        memctx: &MemCtx,
    ) -> Result<()> {
        let many = memctx.read_many(erstba, erstsz).ok_or(
            Error::EventRingSegmentTableLocationInvalid(erstba, erstsz),
        )?;
        self.segment_table = many
            .map(|mut erste: GuestData<EventRingSegment>| {
                // lower bits are reserved
                erste.base_address.0 &= !63;
                if erste.segment_trb_count < 16
                    || erste.segment_trb_count > 4096
                {
                    Err(Error::InvalidEventRingSegmentSize(*erste))
                } else {
                    Ok(*erste)
                }
            })
            .collect::<Result<Vec<EventRingSegment>>>()?;

        if self.segment_table.is_empty() {
            self.enqueue_pointer = None;
            self.segment_remaining_trbs = 0;
        } else if self.enqueue_pointer.is_none() {
            self.enqueue_pointer = Some(self.segment_table[0].base_address);
            self.segment_remaining_trbs =
                self.segment_table[0].segment_trb_count;
        } // XXX: do we do this even if enqueue_ptr *was* previously set?

        Ok(())
    }

    /// Must be called when interrupter's ERDP register is written
    pub fn update_dequeue_pointer(&mut self, erdp: GuestAddr) {
        probes::xhci_producer_ring_set_dequeue_ptr!(|| (erdp.0 as usize));
        self.dequeue_pointer = Some(erdp);
    }

    /// Straight translation of xHCI 1.2 figure 4-12.
    fn is_full(&self) -> bool {
        let deq_ptr = self.dequeue_pointer.unwrap();
        if self.segment_remaining_trbs == 1 {
            // check next segment
            self.next_segment().base_address == deq_ptr
        } else if let Some(enq_ptr) = &self.enqueue_pointer {
            // check current segment
            enq_ptr.offset::<Trb>(1) == deq_ptr
        } else {
            // segment table not initialized yet
            true
        }
    }

    pub fn is_empty(&self) -> bool {
        // XXX: is this actually the way we want to handle Nones?
        let Some(enq_ptr) = &self.enqueue_pointer else { return false };
        let Some(deq_ptr) = &self.dequeue_pointer else { return false };
        enq_ptr == deq_ptr
    }

    /// Straight translation of xHCI 1.2 figure 4-12.
    fn next_segment(&self) -> &EventRingSegment {
        &self.segment_table
            [(self.segment_table_index + 1) % self.segment_table.len()]
    }

    /// Straight translation of xHCI 1.2 figure 4-12.
    fn enqueue_trb_unchecked(&mut self, mut trb: Trb, memctx: &MemCtx) {
        trb.control.set_cycle(self.producer_cycle_state);

        let enq_ptr = self.enqueue_pointer.as_mut().unwrap();
        memctx.write(*enq_ptr, &trb);

        probes::xhci_producer_ring_enqueue_trb!(|| (
            enq_ptr.0 as usize,
            trb.parameter,
            trb.control.trb_type() as u8
        ));

        enq_ptr.0 += size_of::<Trb>() as u64;
        self.segment_remaining_trbs -= 1;

        if self.segment_remaining_trbs == 0 {
            self.segment_table_index += 1;
            if self.segment_table_index >= self.segment_table.len() {
                self.producer_cycle_state = !self.producer_cycle_state;
                self.segment_table_index = 0;
            }
            let erst_entry = &self.segment_table[self.segment_table_index];
            *enq_ptr = erst_entry.base_address;
            self.segment_remaining_trbs = erst_entry.segment_trb_count;
        }
    }

    /// Straight translation of xHCI 1.2 figure 4-12.
    fn enqueue_trb(&mut self, trb: Trb, memctx: &MemCtx) -> Result<()> {
        if self.enqueue_pointer.is_none() || self.segment_remaining_trbs == 0 {
            // ERST is empty, no enqueue pointer set
            Err(Error::EventRingSegmentTableSizeZero(trb))
        } else if self.dequeue_pointer.is_none() {
            // waiting for ERDP write, don't write multiple EventRingFullErrors
            Err(Error::EventRingFull(trb))
        } else if self.is_full() {
            let event_ring_full_error = Trb {
                parameter: 0,
                status: TrbStatusField {
                    event: TrbStatusFieldEvent::default().with_completion_code(
                        TrbCompletionCode::EventRingFullError,
                    ),
                },
                control: TrbControlField {
                    normal: TrbControlFieldNormal::default()
                        .with_trb_type(TrbType::HostControllerEvent),
                },
            };
            self.enqueue_trb_unchecked(event_ring_full_error, memctx);
            // must wait until another ERDP write
            self.dequeue_pointer = None;
            Err(Error::EventRingFull(trb))
        } else {
            self.enqueue_trb_unchecked(trb, memctx);
            Ok(())
        }
    }

    pub fn enqueue(
        &mut self,
        value: EventDescriptor,
        memctx: &MemCtx,
    ) -> Result<()> {
        self.enqueue_trb(value.0, memctx)
    }
}

/// xHCI 1.2 sect 4.11.3: Event Descriptors comprised of only one TRB
#[derive(Debug)]
pub struct EventDescriptor(pub Trb);

#[derive(Debug)]
#[allow(unused)]
pub enum EventInfo {
    Transfer {
        trb_pointer: GuestAddr,
        completion_code: TrbCompletionCode,
        trb_transfer_length: u32,
        slot_id: SlotId,
        endpoint_id: u8,
        event_data: bool,
    },
    CommandCompletion {
        completion_code: TrbCompletionCode,
        slot_id: SlotId,
        cmd_trb_addr: GuestAddr,
    },
    PortStatusChange {
        port_id: PortId,
        completion_code: TrbCompletionCode,
    },
    // optional
    BandwidthRequest,
    // optional, for 'virtualization' (not the kind we're doing)
    Doorbell,
    HostController {
        completion_code: TrbCompletionCode,
    },
    /// Several fields correspond to that of the received USB Device
    /// Notification Transaction Packet (DNTP) (xHCI 1.2 table 6-53)
    DeviceNotification {
        /// Notification Type field of the USB DNTP
        notification_type: u8,
        /// the value of bytes 5 through 0x0B of the USB DNTP
        /// (leave most-significant byte empty)
        // TODO: just union/bitstruct this so we can use a [u8; 7]...
        notification_data: u64,
        completion_code: TrbCompletionCode,
        slot_id: SlotId,
    },
    /// Generated when USBCMD EWE (Enable Wrap Event) set and MFINDEX wraps,
    /// used for periodic transfers with Interrupt and Isoch endpoints.
    /// See xHCI 1.2 sect 4.14.2, 6.4.2.8.
    MfIndexWrap,
}

impl Into<EventDescriptor> for EventInfo {
    fn into(self) -> EventDescriptor {
        match self {
            EventInfo::Transfer {
                trb_pointer,
                completion_code,
                trb_transfer_length,
                slot_id,
                endpoint_id,
                event_data,
            } => EventDescriptor(Trb {
                parameter: trb_pointer.0,
                status: TrbStatusField {
                    event: TrbStatusFieldEvent::default()
                        .with_completion_code(completion_code)
                        .with_completion_parameter(trb_transfer_length),
                },
                control: TrbControlField {
                    transfer_event: TrbControlFieldTransferEvent::default()
                        .with_trb_type(TrbType::TransferEvent)
                        .with_slot_id(slot_id)
                        .with_endpoint_id(endpoint_id)
                        .with_event_data(event_data),
                },
            }),
            // xHCI 1.2 sect 6.4.2.2
            Self::CommandCompletion {
                completion_code: code,
                slot_id,
                cmd_trb_addr,
            } => EventDescriptor(Trb {
                parameter: cmd_trb_addr.0,
                status: TrbStatusField {
                    event: TrbStatusFieldEvent::default()
                        .with_completion_code(code),
                },
                control: TrbControlField {
                    event: TrbControlFieldEvent::default()
                        .with_trb_type(TrbType::CommandCompletionEvent)
                        .with_slot_id(slot_id),
                },
            }),
            EventInfo::PortStatusChange { port_id, completion_code } => {
                EventDescriptor(Trb {
                    parameter: (port_id.as_raw_id() as u64) << 24,
                    status: TrbStatusField {
                        event: TrbStatusFieldEvent::default()
                            .with_completion_code(completion_code),
                    },
                    control: TrbControlField {
                        event: TrbControlFieldEvent::default()
                            .with_trb_type(TrbType::PortStatusChangeEvent),
                    },
                })
            }
            EventInfo::BandwidthRequest => {
                unimplemented!("xhci: Bandwidth Request Event TRB")
            }
            EventInfo::Doorbell => {
                unimplemented!("xhci: Doorbell Event TRB")
            }
            EventInfo::HostController { completion_code } => {
                EventDescriptor(Trb {
                    parameter: 0,
                    status: TrbStatusField {
                        event: TrbStatusFieldEvent::default()
                            .with_completion_code(completion_code),
                    },
                    control: TrbControlField {
                        event: TrbControlFieldEvent::default()
                            .with_trb_type(TrbType::HostControllerEvent),
                    },
                })
            }
            EventInfo::DeviceNotification {
                notification_type,
                notification_data,
                completion_code,
                slot_id,
            } => EventDescriptor(Trb {
                parameter: ((notification_type as u64) << 4)
                    | notification_data << 8,
                status: TrbStatusField {
                    event: TrbStatusFieldEvent::default()
                        .with_completion_code(completion_code),
                },
                control: TrbControlField {
                    event: TrbControlFieldEvent::default()
                        .with_trb_type(TrbType::DeviceNotificationEvent)
                        .with_slot_id(slot_id),
                },
            }),
            EventInfo::MfIndexWrap => {
                EventDescriptor(Trb {
                    parameter: 0,
                    status: TrbStatusField {
                        // always set to Success for MFINDEX Wrap Event.
                        // xHCI 1.2 sect 6.4.2.8
                        event: TrbStatusFieldEvent::default()
                            .with_completion_code(TrbCompletionCode::Success),
                    },
                    control: TrbControlField {
                        event: TrbControlFieldEvent::default()
                            .with_trb_type(TrbType::MfIndexWrapEvent),
                    },
                })
            }
        }
    }
}

pub mod migrate {
    use crate::common::GuestAddr;

    use super::{EventRing, EventRingSegment};
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct EventRingV1 {
        pub enqueue_pointer: Option<u64>,
        pub dequeue_pointer: Option<u64>,
        pub segment_table: Vec<EventRingSegmentV1>,
        pub segment_table_index: usize,
        pub segment_remaining_trbs: usize,
        pub producer_cycle_state: bool,
    }

    impl From<&EventRing> for EventRingV1 {
        fn from(evt_ring: &EventRing) -> Self {
            let EventRing {
                enqueue_pointer,
                dequeue_pointer,
                segment_table,
                segment_table_index,
                segment_remaining_trbs,
                producer_cycle_state,
            } = evt_ring;
            Self {
                enqueue_pointer: enqueue_pointer.map(|x| x.0),
                dequeue_pointer: dequeue_pointer.map(|x| x.0),
                segment_table: segment_table.iter().map(From::from).collect(),
                segment_table_index: *segment_table_index,
                segment_remaining_trbs: *segment_remaining_trbs,
                producer_cycle_state: *producer_cycle_state,
            }
        }
    }

    impl From<&EventRingV1> for EventRing {
        fn from(value: &EventRingV1) -> Self {
            let EventRingV1 {
                enqueue_pointer,
                dequeue_pointer,
                segment_table,
                segment_table_index,
                segment_remaining_trbs,
                producer_cycle_state,
            } = value;
            Self {
                enqueue_pointer: enqueue_pointer.map(GuestAddr),
                dequeue_pointer: dequeue_pointer.map(GuestAddr),
                segment_table: segment_table.iter().map(From::from).collect(),
                segment_table_index: *segment_table_index,
                segment_remaining_trbs: *segment_remaining_trbs,
                producer_cycle_state: *producer_cycle_state,
            }
        }
    }

    #[derive(Deserialize, Serialize)]
    pub struct EventRingSegmentV1 {
        pub base_address: u64,
        pub segment_trb_count: usize,
    }

    impl From<&EventRingSegment> for EventRingSegmentV1 {
        fn from(ers: &EventRingSegment) -> Self {
            let EventRingSegment { base_address, segment_trb_count } = ers;
            Self {
                base_address: base_address.0,
                segment_trb_count: *segment_trb_count,
            }
        }
    }

    impl From<&EventRingSegmentV1> for EventRingSegment {
        fn from(value: &EventRingSegmentV1) -> Self {
            let EventRingSegmentV1 { base_address, segment_trb_count } = value;
            Self {
                base_address: GuestAddr(*base_address),
                segment_trb_count: *segment_trb_count,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::vmm::PhysMap;

    #[test]
    fn test_event_ring_enqueue() {
        let mut phys_map = PhysMap::new_test(16 * 1024);
        phys_map.add_test_mem("guest-ram".to_string(), 0, 16 * 1024).unwrap();
        let memctx = phys_map.memctx();

        let erstba = GuestAddr(0);
        let erstsz = 2;
        let erst_entries = [
            EventRingSegment {
                base_address: GuestAddr(1024),
                segment_trb_count: 16,
            },
            EventRingSegment {
                base_address: GuestAddr(2048),
                segment_trb_count: 16,
            },
        ];

        memctx.write_many(erstba, &erst_entries);

        let erdp = erst_entries[0].base_address;

        let mut ring = EventRing::new(erstba, erstsz, erdp, &memctx).unwrap();

        let mut ed_trb = Trb {
            parameter: 0,
            status: TrbStatusField {
                event: TrbStatusFieldEvent::default()
                    .with_completion_code(TrbCompletionCode::Success),
            },
            control: TrbControlField {
                normal: TrbControlFieldNormal::default()
                    .with_trb_type(TrbType::EventData),
            },
        };
        // enqueue 31 out of 32 (EventRing must leave room for one final
        // event in case of a full ring: the EventRingFullError event!)
        for i in 1..32 {
            ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap();
            ed_trb.parameter = i;
        }
        ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap_err();

        // further additions should do nothing until we write a new ERDP
        ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap_err();

        let mut ring_contents = Vec::new();
        for erste in &erst_entries {
            ring_contents.extend(
                memctx
                    .read_many::<Trb>(
                        erste.base_address,
                        erste.segment_trb_count,
                    )
                    .unwrap(),
            );
        }

        assert_eq!(ring_contents.len(), 32);
        // cycle bits should be set in all these
        for i in 0..31 {
            assert_eq!(ring_contents[i].parameter, i as u64);
            assert_eq!(ring_contents[i].control.trb_type(), TrbType::EventData);
            assert_eq!(ring_contents[i].control.cycle(), true);
            assert_eq!(
                unsafe { ring_contents[i].status.event.completion_code() },
                TrbCompletionCode::Success
            );
        }
        {
            let hce = ring_contents[31];
            assert_eq!(hce.control.cycle(), true);
            assert_eq!(hce.control.trb_type(), TrbType::HostControllerEvent);
            assert_eq!(
                unsafe { hce.status.event.completion_code() },
                TrbCompletionCode::EventRingFullError
            );
        }

        // let's say we (the "software") processed the first 8 events.
        ring.update_dequeue_pointer(
            erst_entries[0].base_address.offset::<Trb>(8),
        );

        // try to enqueue another 8 events!
        for i in 32..39 {
            ed_trb.parameter = i;
            ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap();
        }
        ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap_err();

        // check that they've overwritten previous entries appropriately
        ring_contents.clear();
        for erste in &erst_entries {
            ring_contents.extend(
                memctx
                    .read_many::<Trb>(
                        erste.base_address,
                        erste.segment_trb_count,
                    )
                    .unwrap(),
            );
        }

        // cycle bits should be cleared on the new entries
        for i in 0..7 {
            assert_eq!(ring_contents[i].parameter, 32 + i as u64);
            assert_eq!(ring_contents[i].control.trb_type(), TrbType::EventData);
            assert_eq!(ring_contents[i].control.cycle(), false);
            assert_eq!(
                unsafe { ring_contents[i].status.event.completion_code() },
                TrbCompletionCode::Success
            );
        }
        {
            let hce = ring_contents[7];
            assert_eq!(hce.control.cycle(), false);
            assert_eq!(hce.control.trb_type(), TrbType::HostControllerEvent);
            assert_eq!(
                unsafe { hce.status.event.completion_code() },
                TrbCompletionCode::EventRingFullError
            );

            // haven't overwritten this one (only wrote one EventRingFullError)
            let prev = ring_contents[8];
            assert_eq!(prev.parameter, 8);
            assert_eq!(prev.control.cycle(), true);
            assert_eq!(prev.control.trb_type(), TrbType::EventData);
        }

        // let's say the software processed the rest of the events,
        // and decided it was time to increase the ring size.
        ring.update_dequeue_pointer(
            erst_entries[0].base_address.offset::<Trb>(8),
        );
        // test event ring segment table resizes: write a new entry in the table
        memctx.write(
            erstba + size_of_val(&erst_entries),
            &EventRingSegment {
                base_address: GuestAddr(3072),
                segment_trb_count: 8,
            },
        );
        // and update the ring to use it... wait, they need to be at least 16!
        assert!(matches!(
            ring.update_segment_table(erstba, erstsz + 1, &memctx).unwrap_err(),
            Error::InvalidEventRingSegmentSize(_)
        ));

        // alright, let's try that again
        memctx.write(
            erstba + size_of_val(&erst_entries),
            &EventRingSegment {
                base_address: GuestAddr(3072),
                segment_trb_count: 16,
            },
        );
        // and *now* update the ring to use it
        ring.update_segment_table(erstba, erstsz + 1, &memctx).unwrap();

        // are we no longer full?
        for i in 39..86 {
            ed_trb.parameter = i;
            ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap();
        }
        // but *now* we're full, right?
        assert!(matches!(
            ring.enqueue(EventDescriptor(ed_trb), &memctx).unwrap_err(),
            Error::EventRingFull(_)
        ));
    }
}
