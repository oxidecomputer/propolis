// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers for dealing with configuration space.

use crate::common::RWOp;
use crate::common::ReadOp;
use crate::util::regmap::Flags;
use crate::util::regmap::RegMap;

use super::bits::*;
use super::Cap;

#[derive(Debug)]
pub(super) enum CfgReg {
    Std,
    Custom(u8),
    CapId(u8),
    CapNext(u8),
    CapBody(u8),
}

/// A helper for building maps of PCI device configuration space.
pub(super) struct CfgBuilder {
    cfgmap: RegMap<CfgReg>,
    caps: Vec<Cap>,
    cap_next_alloc: usize,
}

impl CfgBuilder {
    /// Creates a new PCI configuration space map builder.
    pub fn new() -> Self {
        let mut cfgmap = RegMap::new(LEN_CFG_ECAM);
        cfgmap.define_with_flags(0, LEN_CFG_STD, CfgReg::Std, Flags::PASSTHRU);
        Self { cfgmap, caps: Vec::new(), cap_next_alloc: LEN_CFG_STD }
    }

    fn check_overlap(&self, offset: usize, len: usize) {
        let mut buf = [0u8; u8::MAX as usize + 1];
        let mut ro = ReadOp::from_buf(offset, &mut buf);
        self.cfgmap.read(&mut ro, &mut |region, rwo| {
            if let RWOp::Read(ro) = rwo {
                panic!(
                    "New config region at {} with length {} conflicts \
                       with existing region {:?} at offset {} with length {}",
                    offset,
                    len,
                    region,
                    ro.offset(),
                    ro.len()
                );
            } else {
                panic!("Unexpected write operation in check_overlap");
            }
        });
    }

    /// Adds a custom endpoint-defined configuration region at the supplied
    /// offset with the supplied length.
    ///
    /// # Panics
    ///
    /// Panics if the custom region overlaps an existing region in the space
    /// under construction.
    pub fn add_custom(&mut self, offset: u8, len: u8) {
        self.check_overlap(offset as usize, len as usize);
        self.cfgmap.define_with_flags(
            offset as usize,
            len as usize,
            CfgReg::Custom(offset),
            Flags::PASSTHRU,
        );
    }

    /// Adds a new capability region of the supplied length at the next
    /// available offset in configuration space.
    ///
    /// The `len` argument supplies the length of the variable-size portion of
    /// the capability (i.e., the length of the capability data exclusive of the
    /// 1-byte capability ID and next capability pointer registers).
    ///
    /// Note: The builder allocates capability regions in sequence starting
    /// immediately after the config space header. Allocating a custom region
    /// does not advance the builder's "next capability" pointer. The caller is
    /// responsible for arranging config space so that capabilities precede any
    /// allocated custom regions; the builder will panic if it detects any
    /// overlapping regions.
    ///
    /// # Panics
    ///
    /// Panics if:
    ///
    /// - The capability overlaps an existing region in the space under
    ///   construction;
    /// - The capability's total size (inclusive of the capability ID and
    ///   capability pointer registers) is not a multiple of 4 bytes; or
    /// - The capability's total size (again inclusive of the standard
    ///   registers) is 256 bytes or larger.
    pub fn add_capability(&mut self, id: u8, len: u8) {
        self.check_overlap(self.cap_next_alloc, len as usize);
        let end = self.cap_next_alloc + 2 + len as usize;
        // XXX: on the caller to size properly for alignment requirements
        assert!(end % 4 == 0);
        assert!(end <= u8::MAX as usize);
        let idx = self.caps.len() as u8;
        self.caps.push(Cap::new(id, self.cap_next_alloc as u8));
        self.cfgmap.define(self.cap_next_alloc, 1, CfgReg::CapId(idx));
        self.cfgmap.define(self.cap_next_alloc + 1, 1, CfgReg::CapNext(idx));
        self.cfgmap.define(
            self.cap_next_alloc + 2,
            len as usize,
            CfgReg::CapBody(idx),
        );
        self.cap_next_alloc = end;
    }

    /// Constructs the configuration space and a description of its
    /// capabilities.
    pub fn finish(self) -> (RegMap<CfgReg>, Vec<Cap>) {
        (self.cfgmap, self.caps)
    }
}
