// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cmp::Ordering;
use std::ops::Bound::Included;

use super::aspace::ASpace;
use crate::common::*;

#[derive(Debug)]
struct RegDef<ID> {
    id: ID,
    flags: Flags,
}

/// Represents a mapping of registers within an address space.
#[derive(Debug)]
pub struct RegMap<ID> {
    len: usize,
    space: ASpace<RegDef<ID>>,
}

bitflags! {
    #[derive(Default, Debug)]
    pub struct Flags: u8 {
        const DEFAULT = 0;
        const NO_READ_EXTEND = 0b00000001;
        const NO_WRITE_EXTEND = 0b00000010;
        const NO_READ_MOD_WRITE = 0b00000100;
        const PASSTHRU = Self::NO_READ_EXTEND.bits() |
            Self::NO_WRITE_EXTEND.bits();
    }
}

struct RegXfer<'a, ID> {
    reg: &'a RegDef<ID>,
    reg_len: usize,
    offset: usize,
    skip_front_idx: usize,
    split_back_idx: usize,
}

impl<ID> RegMap<ID> {
    pub fn new(len: usize) -> Self {
        Self { len, space: ASpace::new(0, len - 1) }
    }

    pub fn define(&mut self, start: usize, len: usize, id: ID) {
        self.define_with_flags(start, len, id, Flags::DEFAULT)
    }

    pub fn define_with_flags(
        &mut self,
        start: usize,
        len: usize,
        id: ID,
        flags: Flags,
    ) {
        self.space.register(start, len, RegDef { id, flags }).unwrap();
    }

    pub fn process<F>(&self, op: &mut RWOp<'_, '_>, mut f: F)
    where
        F: FnMut(&ID, RWOp),
    {
        match op {
            RWOp::Read(ro) => {
                self.read(ro, &mut f);
            }
            RWOp::Write(wo) => {
                self.write(wo, &mut f);
            }
        }
    }
    pub fn read<F>(&self, ro: &mut ReadOp, f: &mut F)
    where
        F: FnMut(&ID, RWOp),
    {
        assert!(ro.len() != 0);
        assert!(ro.offset() + ro.len() - 1 < self.len);

        self.iterate_transfers(ro.offset(), ro.len(), |xfer: &RegXfer<ID>| {
            let mut copy_op = ReadOp::new_child(
                xfer.offset,
                ro,
                xfer.skip_front_idx..xfer.split_back_idx,
            );

            debug_assert!(copy_op.len() != 0);
            Self::reg_read(xfer.reg, xfer.reg_len, &mut copy_op, f);
        })
    }

    pub fn write<F>(&self, wo: &mut WriteOp, f: &mut F)
    where
        F: FnMut(&ID, RWOp),
    {
        assert!(wo.len() != 0);
        assert!(wo.offset() + wo.len() - 1 < self.len);

        self.iterate_transfers(wo.offset(), wo.len(), |xfer| {
            let mut copy_op = WriteOp::new_child(
                xfer.offset,
                wo,
                xfer.skip_front_idx..xfer.split_back_idx,
            );

            debug_assert!(copy_op.len() != 0);
            Self::reg_write(xfer.reg, xfer.reg_len, &mut copy_op, f);
        })
    }

    fn reg_read<F>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &mut ReadOp,
        f: &mut F,
    ) where
        F: FnMut(&ID, RWOp),
    {
        if reg.flags.contains(Flags::NO_READ_EXTEND) && reg_len != copy_op.len()
        {
            f(&reg.id, RWOp::Read(copy_op));
        } else if reg_len == copy_op.len() {
            debug_assert!(copy_op.offset() == 0);
            f(&reg.id, RWOp::Read(copy_op));
        } else {
            let mut scratch = vec![0; reg_len];
            let mut sro = ReadOp::from_buf(0, &mut scratch);

            f(&reg.id, RWOp::Read(&mut sro));
            drop(sro);
            copy_op.write_bytes(
                &scratch[copy_op.offset()..(copy_op.offset() + copy_op.len())],
            );
        }
    }

    fn reg_write<F>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &mut WriteOp,
        f: &mut F,
    ) where
        F: FnMut(&ID, RWOp),
    {
        if reg.flags.contains(Flags::NO_WRITE_EXTEND)
            && reg_len != copy_op.len()
        {
            f(&reg.id, RWOp::Write(copy_op));
        } else if reg_len == copy_op.len() {
            debug_assert!(copy_op.offset() == 0);
            f(&reg.id, RWOp::Write(copy_op));
        } else {
            let mut scratch = vec![0; reg_len];

            if !reg.flags.contains(Flags::NO_READ_MOD_WRITE) {
                let mut sro = ReadOp::from_buf(0, &mut scratch);
                f(&reg.id, RWOp::Read(&mut sro));
            }
            copy_op.read_bytes(
                &mut scratch
                    [copy_op.offset()..(copy_op.offset() + copy_op.len())],
            );

            f(&reg.id, RWOp::Write(&mut WriteOp::from_buf(0, &scratch)));
        }
    }

    fn iterate_transfers<F>(&self, offset: usize, len: usize, mut do_xfer: F)
    where
        F: FnMut(&RegXfer<'_, ID>),
    {
        let last_position = offset + len - 1;
        let mut position = offset;

        assert!(len != 0);
        assert!(last_position < self.len);

        for (reg_start, reg_len, reg) in
            self.space.covered_by((Included(offset), Included(last_position)))
        {
            let mut skip_front = 0;
            let mut split_back = 0;
            let mut reg_offset = 0;

            let consumed = position - offset;
            let remain = len - consumed;

            match position.cmp(&reg_start) {
                Ordering::Equal => {
                    if remain > reg_len {
                        split_back = remain - reg_len;
                    }
                }
                Ordering::Less => {
                    debug_assert!(position + remain > reg_start);
                    skip_front = reg_start - position;
                    if remain - skip_front > reg_len {
                        split_back = remain - (skip_front + reg_len);
                    }
                }
                Ordering::Greater => {
                    reg_offset = position - reg_start;
                    if reg_offset + remain > reg_len {
                        split_back = reg_offset + remain - reg_len;
                    }
                }
            };
            let xfer_len = remain - (skip_front + split_back);
            debug_assert!(xfer_len <= reg_len);

            do_xfer(&RegXfer {
                reg,
                reg_len,
                offset: reg_offset,
                skip_front_idx: consumed + skip_front,
                split_back_idx: consumed + skip_front + xfer_len,
            });

            position = reg_start + reg_offset + xfer_len;
        }
    }
}
impl<ID: Copy + Eq> RegMap<ID> {
    pub fn create_packed(
        size: usize,
        regdef: &[(ID, usize)],
        resv_reg: Option<ID>,
    ) -> Self {
        RegMap::create_packed_iter(size, regdef.iter().copied(), resv_reg)
    }
    pub fn create_packed_iter(
        size: usize,
        regdef: impl IntoIterator<Item = (ID, usize)>,
        resv_reg: Option<ID>,
    ) -> Self {
        let mut map = RegMap::new(size);
        let mut off = 0;
        for reg in regdef {
            let (id, reg_size) = (reg.0, reg.1);
            let flags = match resv_reg.as_ref() {
                Some(resv) if *resv == id => {
                    Flags::NO_READ_EXTEND | Flags::NO_WRITE_EXTEND
                }
                _ => Flags::DEFAULT,
            };
            map.define_with_flags(off, reg_size, id, flags);
            off += reg_size;
        }
        assert_eq!(size, off);

        map
    }
    pub fn create_packed_passthru(size: usize, regdef: &[(ID, usize)]) -> Self {
        let mut map = RegMap::new(size);
        let mut off = 0;
        for reg in regdef.iter() {
            let (id, reg_size) = (reg.0, reg.1);
            map.define_with_flags(off, reg_size, id, Flags::PASSTHRU);
            off += reg_size;
        }
        assert_eq!(size, off);

        map
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Clone, Copy, Eq, PartialEq, Debug)]
    enum XferDir {
        Read,
        Write,
    }
    #[derive(Clone, Copy, Eq, PartialEq, Debug)]
    struct Xfer<ID: Copy + Eq> {
        dir: XferDir,
        reg: ID,
        off: usize,
        len: usize,
    }
    impl<ID: Copy + Eq> Xfer<ID> {
        fn from_rwo(id: &ID, rwo: RWOp) -> Self {
            let dir = match rwo {
                RWOp::Read(_) => XferDir::Read,
                RWOp::Write(_) => XferDir::Write,
            };
            Xfer { dir, reg: *id, off: rwo.offset(), len: rwo.len() }
        }
        fn read(id: ID, off: usize, len: usize) -> Self {
            Xfer { dir: XferDir::Read, reg: id, off, len }
        }
        #[allow(unused)]
        fn write(id: ID, off: usize, len: usize) -> Self {
            Xfer { dir: XferDir::Write, reg: id, off, len }
        }
    }

    fn read(off: usize, len: usize, cb: impl FnOnce(RWOp)) {
        let mut buf = vec![0; len];
        let mut ro = ReadOp::from_buf(off, &mut buf);
        cb(RWOp::Read(&mut ro))
    }
    #[allow(unused)]
    fn write(off: usize, len: usize, cb: impl FnOnce(&mut RWOp)) {
        let mut buf = vec![0; len];
        let mut wo = WriteOp::from_buf(off, &buf);
        cb(&mut RWOp::Write(&mut wo))
    }
    fn drive_reads<ID: Copy + Eq>(
        xfers: &[(usize, usize)],
        map: &RegMap<ID>,
    ) -> Vec<Xfer<ID>> {
        let mut res = Vec::new();

        for &(off, len) in xfers {
            read(off, len, |mut rwo| {
                map.process(&mut rwo, |id, rwo| {
                    res.push(Xfer::from_rwo(id, rwo))
                })
            })
        }
        res
    }

    #[test]
    fn simple() {
        // Simple map with varied sizing
        let map = RegMap::create_packed(
            0x10,
            &[('a', 1), ('b', 1), ('c', 2), ('d', 4), ('e', 8)],
            None,
        );
        let expected = vec![
            Xfer::read('a', 0, 1),
            Xfer::read('b', 0, 1),
            Xfer::read('c', 0, 2),
            Xfer::read('d', 0, 4),
            Xfer::read('e', 0, 8),
        ];
        // Each field individually
        let reads = [(0, 1), (1, 1), (2, 2), (4, 4), (8, 8)];
        let res = drive_reads(&reads, &map);
        assert_eq!(res, expected);
        // One big op, covering all
        let reads = [(0, 0x10)];
        let res = drive_reads(&reads, &map);
        assert_eq!(res, expected);
    }
    #[test]
    fn misaligned() {
        // Map shaped like virtio-net config with weird offsets due to mac addr
        let map = RegMap::create_packed(
            12,
            &[('a', 6), ('b', 2), ('c', 2), ('d', 2)],
            None,
        );

        let expected = vec![
            Xfer::read('a', 0, 6),
            Xfer::read('a', 0, 6),
            Xfer::read('b', 0, 2),
            Xfer::read('c', 0, 2),
            Xfer::read('d', 0, 2),
        ];
        // Each field individually with 4-byte reads
        let reads = [(0, 4), (4, 4), (8, 4)];
        let res = drive_reads(&reads, &map);
        assert_eq!(res, expected);
    }
}
