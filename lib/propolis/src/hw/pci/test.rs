// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::accessors::*;
use crate::common::{RWOp, ReadOp, WriteOp};
use crate::mmio::MmioBus;
use crate::pio::PioBus;

use super::bus::Bus;
use super::topology::{self, Topology};
use super::{bits, Bdf, PcieCfgDecoder};

// Common test prep setup

pub(crate) struct Scaffold {
    pub bus_mmio: Arc<MmioBus>,
    pub bus_pio: Arc<PioBus>,
    pub acc_mem: MemAccessor,
    pub acc_msi: MsiAccessor,
}
impl Scaffold {
    pub(crate) fn new() -> Self {
        Self {
            bus_mmio: Arc::new(MmioBus::new(u32::MAX as usize)),
            bus_pio: Arc::new(PioBus::new()),
            acc_mem: MemAccessor::new_orphan(),
            acc_msi: MsiAccessor::new_orphan(),
        }
    }

    pub(crate) fn create_bus(&self) -> Bus {
        Bus::new(
            &self.bus_pio,
            &self.bus_mmio,
            self.acc_mem.child(None),
            self.acc_msi.child(None),
        )
    }

    pub(crate) fn basic_topo(&self) -> Arc<Topology> {
        topology::Topology::new_test(self.create_bus())
    }
}

// PCI-generic tests

#[test]
fn pcie_decoder() {
    let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
    let mut buf = [0u8; 4];
    let mut ro = ReadOp::from_buf(0, &mut buf);
    pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
        assert_eq!(*bdf, Bdf::new(0, 0, 0).unwrap());
        assert!(matches!(rwo, RWOp::Read(_)));
        assert_eq!(rwo.offset(), 0);
        assert_eq!(rwo.len(), 4);
        Some(())
    });

    let buf = [0u8; 16];
    let mut wo = WriteOp::from_buf(0x400, &buf);
    pcie.service(RWOp::Write(&mut wo), |bdf, rwo| {
        assert_eq!(*bdf, Bdf::new(0, 0, 0).unwrap());
        assert!(matches!(rwo, RWOp::Write(_)));
        assert_eq!(rwo.offset(), 0x400);
        assert_eq!(rwo.len(), 16);
        Some(())
    })
}

#[test]
fn pcie_decoder_multiple_bdfs() {
    let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
    let mut buf = [0u8; 4];
    let mut ro = ReadOp::from_buf(1_usize << 12, &mut buf);
    pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
        assert_eq!(*bdf, Bdf::new(0, 0, 1).unwrap());
        assert_eq!(rwo.offset(), 0);
        Some(())
    });

    let mut ro =
        ReadOp::from_buf(4_usize << 15 | 3_usize << 12 | 0x123, &mut buf);
    pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
        assert_eq!(*bdf, Bdf::new(0, 4, 3).unwrap());
        assert_eq!(rwo.offset(), 0x123);
        Some(())
    });

    let mut ro = ReadOp::from_buf(
        133_usize << 20 | 7_usize << 15 | 1_usize << 12 | 0x337,
        &mut buf,
    );
    pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
        assert_eq!(*bdf, Bdf::new(133, 7, 1).unwrap());
        assert_eq!(rwo.offset(), 0x337);
        Some(())
    });
}

#[test]
fn pcie_decoder_min_buses() {
    let pcie = PcieCfgDecoder::new(4);
    let mut buf = [0u8; 4];
    for seg_group in 0..4 {
        for bus in 0..4 {
            let mut ro =
                ReadOp::from_buf((seg_group * 4 + bus) << 20, &mut buf);
            pcie.service(RWOp::Read(&mut ro), |bdf, rwo| {
                assert_eq!(
                    *bdf,
                    Bdf::new(bus as u8, 0, 0).unwrap(),
                    "group {}, bus {}",
                    seg_group,
                    bus
                );
                assert_eq!(rwo.offset(), 0);
                Some(())
            });
        }
    }
}

#[test]
fn pcie_decoder_access_spans_multiple_devs() {
    let pcie = PcieCfgDecoder::new(bits::PCIE_MAX_BUSES_PER_ECAM_REGION);
    let mut buf = [0u8; 8];
    let mut ro = ReadOp::from_buf(0xffc, &mut buf);

    // This access spans multiple functions, so the decoder can't
    // meaningfully address a single BDF and should therefore not
    // invoke the closure.
    pcie.service(RWOp::Read(&mut ro), |_bdf, _rwo| panic!());
    assert_eq!(buf, [0xffu8; 8]);
}
