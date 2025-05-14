// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::marker::PhantomData;

use crate::common::GuestAddr;
use crate::hw::usb::xhci::bits::ring_data::*;
use crate::vmm::MemCtx;

pub mod command;
pub mod transfer;

pub mod doorbell;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn xhci_consumer_ring_dequeue_trb(offset: usize, data: u64, trb_type: u8) {}
    fn xhci_consumer_ring_set_dequeue_ptr(ptr: usize, cycle_state: bool) {}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error in xHC Ring: {0:?}")]
    IoError(#[from] std::io::Error),
    #[error("Tried to construct Command Descriptor from multiple TRBs")]
    CommandDescriptorSize,
    // XXX: the spec says this, but FreeBSD trips this error;
    // either my understanding or their xHCD may be slightly wrong
    // #[error("Guest defined a consumer TRB ring larger than 64K bytes")]
    // RingTooLarge,
    #[error("Failed reading TRB from guest memory at {0:?}")]
    FailedReadingTRB(GuestAddr),
    #[error("Incomplete TD: no more TRBs in cycle to complete chain: {0:?}")]
    IncompleteWorkItem(Vec<Trb>),
    #[error("Incomplete TD: TRBs with chain bit set formed a full ring circuit: {0:?}")]
    IncompleteWorkItemChainCyclic(Vec<Trb>),
    #[error("TRB Ring Dequeue Pointer was not aligned to size_of<Trb>: {0:?}")]
    InvalidDequeuePointer(GuestAddr),
    #[error("Invalid TRB type for a Command Descriptor: {0:?}")]
    InvalidCommandDescriptor(Trb),
    #[error("Tried to parse empty Transfer Descriptor")]
    EmptyTransferDescriptor,
    #[error("Invalid TRB type for a Transfer Descriptor: {0:?}")]
    InvalidTransferDescriptor(Trb),
    #[error("Unexpected TRB type {0:?}, expected {1:?}")]
    WrongTrbType(TrbType, TrbType),
    #[error("Encountered a complete circuit of matching cycle bits in TRB consumer ring")]
    CompleteCircuitOfMatchingCycleBits,
    #[error("Apparent corrupt Link TRB pointer (lower 4 bits nonzero): *{0:?} = {1:#x}")]
    LinkTRBAlignment(GuestAddr, u64),
}
pub type Result<T> = core::result::Result<T, Error>;

pub struct ConsumerRing<T: WorkItem> {
    // where the ring *starts*, but note that it may be disjoint via Link TRBs
    start_addr: GuestAddr,
    dequeue_ptr: GuestAddr,
    consumer_cycle_state: bool,
    _ghost: PhantomData<T>,
}

pub trait WorkItem: Sized + IntoIterator<Item = Trb> {
    fn try_from_trb_iter(trbs: impl IntoIterator<Item = Trb>) -> Result<Self>;
}

fn check_aligned_addr(addr: GuestAddr) -> Result<()> {
    if addr.0 as usize % size_of::<Trb>() != 0 {
        Err(Error::InvalidDequeuePointer(addr))
    } else {
        Ok(())
    }
}

/// See xHCI 1.2 section 4.14 "Managing Transfer Rings"
impl<T: WorkItem> ConsumerRing<T> {
    pub fn new(addr: GuestAddr, cycle_state: bool) -> Result<Self> {
        check_aligned_addr(addr)?;

        Ok(Self {
            start_addr: addr,
            dequeue_ptr: addr,
            consumer_cycle_state: cycle_state,
            _ghost: PhantomData,
        })
    }

    fn current_trb(&mut self, memctx: &MemCtx) -> Result<Trb> {
        memctx
            .read(self.dequeue_ptr)
            .map(|x| *x)
            .ok_or(Error::FailedReadingTRB(self.dequeue_ptr))
    }

    fn queue_advance(&mut self, memctx: &MemCtx) -> Result<()> {
        let trb = self.current_trb(memctx)?;

        // xHCI 1.2 figure 4-7
        self.dequeue_ptr = if trb.control.trb_type() == TrbType::Link {
            if unsafe { trb.control.link.toggle_cycle() } {
                // xHCI 1.2 figure 4-8
                self.consumer_cycle_state = !self.consumer_cycle_state;
            }

            // xHCI 1.2 sect 4.11.5.1: "The Ring Segment Pointer field in a Link TRB
            // is not required to point to the beginning of a physical memory page."
            // (They *are* required to be at least 16-byte aligned, i.e. sizeof::<TRB>())
            // xHCI 1.2 figure 6-38: lower 4 bits are RsvdZ, so we can ignore them;
            // but they may be a good indicator of error (pointing at garbage memory)
            if trb.parameter & 0b1111 != 0 {
                return Err(Error::LinkTRBAlignment(
                    self.dequeue_ptr,
                    trb.parameter,
                ));
            }
            GuestAddr(trb.parameter & !0b1111)
        } else {
            self.dequeue_ptr.offset::<Trb>(1)
        };

        // xHCI 1.2 sect 4.9: "TRB Rings may be larger than a Page,
        // however they shall not cross a 64K byte boundary."
        if self.dequeue_ptr.0.abs_diff(self.start_addr.0) > 65536 {
            // XXX: FreeBSD seems to have problems with this
            // return Err(Error::RingTooLarge);
        }

        Ok(())
    }

    /// xHCI 1.2 sects 4.6.10, 6.4.3.9
    pub fn set_dequeue_pointer_and_cycle(
        &mut self,
        deq_ptr: GuestAddr,
        cycle_state: bool,
    ) -> Result<()> {
        check_aligned_addr(deq_ptr)?;

        probes::xhci_consumer_ring_set_dequeue_ptr!(|| (
            deq_ptr.0 as usize,
            cycle_state
        ));

        // xHCI 1.2 sect 4.9.2: When a Transfer Ring is enabled or reset,
        // the xHC initializes its copies of the Enqueue and Dequeue Pointers
        // with the value of the Endpoint/Stream Context TR Dequeue Pointer field.
        self.start_addr = deq_ptr;
        self.dequeue_ptr = deq_ptr;
        self.consumer_cycle_state = cycle_state;

        Ok(())
    }

    /// Return the guest address corresponding to the current dequeue pointer.
    pub fn current_dequeue_pointer(&self) -> GuestAddr {
        self.dequeue_ptr
    }

    pub fn consumer_cycle_state(&self) -> bool {
        self.consumer_cycle_state
    }

    /// Find the first transfer-related TRB, if one exists.
    /// (See xHCI 1.2 sect 4.9.2)
    fn dequeue_trb(&mut self, memctx: &MemCtx) -> Result<Option<Trb>> {
        let start_deq_ptr = self.dequeue_ptr;
        loop {
            let trb = self.current_trb(memctx)?;

            // cycle bit transition - found enqueue pointer
            if trb.control.cycle() != self.consumer_cycle_state {
                return Ok(None);
            }

            self.queue_advance(memctx)?;

            if trb.control.trb_type() != TrbType::Link {
                probes::xhci_consumer_ring_dequeue_trb!(|| (
                    self.dequeue_ptr.0 as usize,
                    trb.parameter,
                    trb.control.trb_type() as u8
                ));
                return Ok(Some(trb));
            }
            // failsafe - in case of full circuit of matching cycle bits
            // without a toggle_cycle occurring, avoid infinite loop
            if self.dequeue_ptr == start_deq_ptr {
                return Err(Error::CompleteCircuitOfMatchingCycleBits);
            }
        }
    }

    pub fn dequeue_work_item(&mut self, memctx: &MemCtx) -> Result<T> {
        let start_deq_ptr = self.dequeue_ptr;
        let mut trbs: Vec<Trb> =
            self.dequeue_trb(memctx)?.into_iter().collect();
        while trbs
            .last()
            .and_then(|end_trb| end_trb.control.chain_bit())
            .unwrap_or(false)
        {
            // failsafe - if full circuit of chain bits causes an incomplete work item
            if self.dequeue_ptr == start_deq_ptr {
                return Err(Error::IncompleteWorkItemChainCyclic(trbs));
            }
            if let Some(trb) = self.dequeue_trb(memctx)? {
                trbs.push(trb);
            } else {
                // we need more TRBs for this work item that aren't here yet!
                return Err(Error::IncompleteWorkItem(trbs));
            }
        }
        T::try_from_trb_iter(trbs)
    }

    pub fn export(&self) -> migrate::ConsumerRingV1 {
        let Self { start_addr, dequeue_ptr, consumer_cycle_state, _ghost } =
            self;
        migrate::ConsumerRingV1 {
            start_addr: start_addr.0,
            dequeue_ptr: dequeue_ptr.0,
            consumer_cycle_state: *consumer_cycle_state,
        }
    }

    pub fn import(&mut self, value: &migrate::ConsumerRingV1) {
        let migrate::ConsumerRingV1 {
            start_addr,
            dequeue_ptr,
            consumer_cycle_state,
        } = value;
        self.start_addr = GuestAddr(*start_addr);
        self.dequeue_ptr = GuestAddr(*dequeue_ptr);
        self.consumer_cycle_state = *consumer_cycle_state;
    }
}

impl<T: WorkItem> TryFrom<&migrate::ConsumerRingV1> for ConsumerRing<T> {
    type Error = crate::migrate::MigrateStateError;
    fn try_from(
        value: &migrate::ConsumerRingV1,
    ) -> std::result::Result<Self, Self::Error> {
        let migrate::ConsumerRingV1 {
            start_addr,
            dequeue_ptr,
            consumer_cycle_state,
        } = value;
        let mut ring = Self::new(GuestAddr(*start_addr), *consumer_cycle_state)
            .map_err(|e| {
                crate::migrate::MigrateStateError::ImportFailed(format!(
                    "Consumer ring address error: {e}"
                ))
            })?;
        ring.dequeue_ptr = GuestAddr(*dequeue_ptr);
        Ok(ring)
    }
}

pub mod migrate {
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct ConsumerRingV1 {
        pub start_addr: u64,
        pub dequeue_ptr: u64,
        pub consumer_cycle_state: bool,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        hw::usb::usbdev::requests::{
            Request, RequestDirection, RequestRecipient, RequestType,
            SetupData, StandardRequest,
        },
        vmm::PhysMap,
    };
    use transfer::TransferRing;

    #[test]
    fn test_get_device_descriptor_transfer_ring() {
        let mut phys_map = PhysMap::new_test(16 * 1024);
        phys_map.add_test_mem("guest-ram".to_string(), 0, 16 * 1024).unwrap();
        let memctx = phys_map.memctx();

        // mimicking pg. 85 of xHCI 1.2, but with Links thrown in
        let ring_segments: &[&[_]] = &[
            &[
                // setup stage
                Trb {
                    parameter: SetupData(0)
                        .with_direction(RequestDirection::DeviceToHost)
                        .with_request_type(RequestType::Standard)
                        .with_recipient(RequestRecipient::Device)
                        .with_request(Request::Standard(
                            StandardRequest::GetDescriptor,
                        ))
                        .with_value(0x100)
                        .with_index(0)
                        .with_length(8)
                        .0,
                    status: TrbStatusField {
                        transfer: TrbStatusFieldTransfer::default()
                            .with_trb_transfer_length(8)
                            .with_interrupter_target(0),
                    },
                    control: TrbControlField {
                        setup_stage: TrbControlFieldSetupStage::default()
                            .with_cycle(true)
                            .with_immediate_data(true)
                            .with_trb_type(TrbType::SetupStage)
                            .with_transfer_type(TrbTransferType::InDataStage),
                    },
                },
                // data stage
                Trb {
                    parameter: 0x123456789abcdef0u64,
                    status: TrbStatusField {
                        transfer: TrbStatusFieldTransfer::default()
                            .with_trb_transfer_length(8),
                    },
                    control: TrbControlField {
                        data_stage: TrbControlFieldDataStage::default()
                            .with_cycle(true)
                            .with_trb_type(TrbType::DataStage)
                            .with_direction(TrbDirection::In),
                    },
                },
                // link to next ring segment
                Trb {
                    parameter: 2048,
                    status: TrbStatusField::default(),
                    control: TrbControlField {
                        link: TrbControlFieldLink::default()
                            .with_cycle(true)
                            .with_trb_type(TrbType::Link),
                    },
                },
            ],
            &[
                // status stage
                Trb {
                    parameter: 0,
                    status: TrbStatusField::default(),
                    control: TrbControlField {
                        status_stage: TrbControlFieldStatusStage::default()
                            .with_cycle(true)
                            .with_interrupt_on_completion(true)
                            .with_trb_type(TrbType::StatusStage)
                            .with_direction(TrbDirection::Out),
                    },
                },
                // link back to first ring segment (with toggle cycle)
                Trb {
                    parameter: 1024,
                    status: TrbStatusField::default(),
                    control: TrbControlField {
                        link: TrbControlFieldLink::default()
                            .with_toggle_cycle(true)
                            .with_trb_type(TrbType::Link),
                    },
                },
            ],
        ];

        for (i, seg) in ring_segments.iter().enumerate() {
            memctx.write_many(GuestAddr((i as u64 + 1) * 1024), seg);
        }

        let mut ring = TransferRing::new(GuestAddr(1024), true).unwrap();

        let setup_td = ring.dequeue_work_item(&memctx).unwrap();

        assert_eq!(
            ring.current_dequeue_pointer(),
            GuestAddr(1024).offset::<Trb>(1)
        );

        let data_td = ring.dequeue_work_item(&memctx).unwrap();

        assert_eq!(
            ring.current_dequeue_pointer(),
            GuestAddr(1024).offset::<Trb>(2)
        );

        // test setting the dequeue pointer
        ring.set_dequeue_pointer_and_cycle(
            GuestAddr(1024).offset::<Trb>(1),
            true,
        )
        .unwrap();

        assert_eq!(
            ring.current_dequeue_pointer(),
            GuestAddr(1024).offset::<Trb>(1)
        );

        let data_td_copy = ring.dequeue_work_item(&memctx).unwrap();

        assert_eq!(data_td.trb0_type(), data_td_copy.trb0_type());

        assert_eq!(
            ring.current_dequeue_pointer(),
            GuestAddr(1024).offset::<Trb>(2)
        );

        let status_td = ring.dequeue_work_item(&memctx).unwrap();

        // skips link trbs
        assert_eq!(
            ring.current_dequeue_pointer(),
            GuestAddr(2048).offset::<Trb>(1)
        );

        assert!(ring.dequeue_work_item(&memctx).is_ok());

        assert_eq!(setup_td.trbs.len(), 1);
        assert_eq!(data_td.trbs.len(), 1);
        assert_eq!(status_td.trbs.len(), 1);

        assert_eq!(setup_td.trb0_type().unwrap(), TrbType::SetupStage);
        assert_eq!(data_td.trb0_type().unwrap(), TrbType::DataStage);
        assert_eq!(status_td.trb0_type().unwrap(), TrbType::StatusStage);

        assert_eq!(data_td.transfer_size(), 8);
    }

    // test chained TD
    #[test]
    fn test_get_chained_td() {
        let mut phys_map = PhysMap::new_test(16 * 1024);
        phys_map.add_test_mem("guest-ram".to_string(), 0, 16 * 1024).unwrap();
        let memctx = phys_map.memctx();

        let status =
            TrbStatusField { transfer: TrbStatusFieldTransfer::default() };
        let ring_segments: &[&[_]] = &[
            &[
                Trb {
                    parameter: 0,
                    status,
                    control: TrbControlField {
                        data_stage: TrbControlFieldDataStage::default()
                            .with_cycle(true)
                            .with_trb_type(TrbType::DataStage)
                            .with_direction(TrbDirection::Out)
                            .with_chain_bit(true),
                    },
                },
                Trb {
                    parameter: 0,
                    status,
                    control: TrbControlField {
                        normal: TrbControlFieldNormal::default()
                            .with_trb_type(TrbType::Normal)
                            .with_cycle(true)
                            .with_chain_bit(true),
                    },
                },
                // link to next ring segment
                Trb {
                    parameter: 2048,
                    status,
                    control: TrbControlField {
                        link: TrbControlFieldLink::default()
                            .with_cycle(true)
                            .with_trb_type(TrbType::Link),
                    },
                },
            ],
            &[
                Trb {
                    parameter: 0,
                    status,
                    control: TrbControlField {
                        normal: TrbControlFieldNormal::default()
                            // NOTICE: cycle bit change! we'll test incomplete TD first
                            .with_cycle(false),
                    },
                },
                // link back to first ring segment (with toggle cycle)
                Trb {
                    parameter: 1024,
                    status: TrbStatusField::default(),
                    control: TrbControlField {
                        link: TrbControlFieldLink::default()
                            .with_toggle_cycle(true)
                            .with_trb_type(TrbType::Link),
                    },
                },
            ],
        ];

        for (i, seg) in ring_segments.iter().enumerate() {
            memctx.write_many(GuestAddr((i as u64 + 1) * 1024), seg);
        }

        let mut ring = TransferRing::new(GuestAddr(1024), true).unwrap();

        let Error::IncompleteWorkItem(incomplete_td) =
            ring.dequeue_work_item(&memctx).unwrap_err()
        else {
            panic!("wrong error returned for incomplete TD!");
        };

        assert_eq!(incomplete_td.len(), 2);
        assert_eq!(incomplete_td[0].control.trb_type(), TrbType::DataStage);
        assert_eq!(incomplete_td[1].control.trb_type(), TrbType::Normal);

        // complete the TD (cycle match, chain unset)
        memctx.write(
            GuestAddr(2048),
            &Trb {
                parameter: 0,
                status,
                control: TrbControlField {
                    normal: TrbControlFieldNormal::default()
                        .with_trb_type(TrbType::Normal)
                        .with_cycle(true),
                },
            },
        );
        ring.set_dequeue_pointer_and_cycle(GuestAddr(1024), true).unwrap();

        let complete_td = ring.dequeue_work_item(&memctx).unwrap().trbs;
        assert_eq!(complete_td.len(), 3);
        assert_eq!(complete_td[0].control.trb_type(), TrbType::DataStage);
        assert_eq!(complete_td[1].control.trb_type(), TrbType::Normal);
        assert_eq!(complete_td[2].control.trb_type(), TrbType::Normal);
    }
}
