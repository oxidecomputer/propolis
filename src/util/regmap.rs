use std::cmp::Ordering;
use std::ops::Bound::Included;

use super::aspace::ASpace;
use crate::common::*;

struct RegDef<ID> {
    id: ID,
    flags: Flags,
}

pub struct RegMap<ID> {
    len: usize,
    space: ASpace<RegDef<ID>>,
}

bitflags! {
    #[derive(Default)]
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
        F: FnMut(&ID, &mut RWOp),
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
        F: FnMut(&ID, &mut RWOp),
    {
        assert!(!ro.buf.is_empty());
        assert!(ro.offset + ro.buf.len() - 1 < self.len);

        let buf_len = ro.buf.len();
        let buf = &mut ro.buf;
        self.iterate_transfers(ro.offset, buf_len, |xfer: &RegXfer<ID>| {
            let buf_xfer = &mut buf[xfer.skip_front_idx..xfer.split_back_idx];
            let mut copy_op = ReadOp::new(xfer.offset, buf_xfer);

            debug_assert!(!copy_op.buf.is_empty());
            Self::reg_read(xfer.reg, xfer.reg_len, &mut copy_op, f);
        })
    }

    pub fn write<F>(&self, wo: &WriteOp, f: &mut F)
    where
        F: FnMut(&ID, &mut RWOp),
    {
        assert!(!wo.buf.is_empty());
        assert!(wo.offset + wo.buf.len() - 1 < self.len);

        let buf_len = wo.buf.len();
        let buf = wo.buf;
        self.iterate_transfers(wo.offset, buf_len, |xfer| {
            let buf_xfer = &buf[xfer.skip_front_idx..xfer.split_back_idx];
            let copy_op = WriteOp::new(xfer.offset, buf_xfer);

            debug_assert!(!copy_op.buf.is_empty());
            Self::reg_write(xfer.reg, xfer.reg_len, &copy_op, f);
        })
    }

    fn reg_read<F>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &mut ReadOp,
        f: &mut F,
    ) where
        F: FnMut(&ID, &mut RWOp),
    {
        if reg.flags.contains(Flags::NO_READ_EXTEND)
            && reg_len != copy_op.buf.len()
        {
            f(&reg.id, &mut RWOp::Read(copy_op));
        } else if reg_len == copy_op.buf.len() {
            debug_assert!(copy_op.offset == 0);
            f(&reg.id, &mut RWOp::Read(copy_op));
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            f(&reg.id, &mut RWOp::Read(&mut ReadOp::new(0, &mut scratch)));
            copy_op.buf.copy_from_slice(
                &scratch[copy_op.offset..(copy_op.offset + copy_op.buf.len())],
            );
        }
    }

    fn reg_write<F>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &WriteOp,
        f: &mut F,
    ) where
        F: FnMut(&ID, &mut RWOp),
    {
        if reg.flags.contains(Flags::NO_WRITE_EXTEND)
            && reg_len != copy_op.buf.len()
        {
            f(&reg.id, &mut RWOp::Write(copy_op));
        } else if reg_len == copy_op.buf.len() {
            debug_assert!(copy_op.offset == 0);
            f(&reg.id, &mut RWOp::Write(copy_op));
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            if !reg.flags.contains(Flags::NO_READ_MOD_WRITE) {
                f(&reg.id, &mut RWOp::Read(&mut ReadOp::new(0, &mut scratch)));
            }
            scratch[copy_op.offset..(copy_op.offset + copy_op.buf.len())]
                .copy_from_slice(copy_op.buf);

            f(&reg.id, &mut RWOp::Write(&WriteOp::new(0, &scratch)));
        }
    }

    fn iterate_transfers<F>(&self, offset: usize, len: usize, mut do_xfer: F)
    where
        F: FnMut(&RegXfer<'_, ID>),
    {
        let last_position = offset + len - 1;
        let mut position = offset;

        // how much of the transfer length is remaining (or conversely, been consumed)
        let mut remain = len;
        let mut consumed = 0;

        assert!(len != 0);
        assert!(last_position < self.len);

        for (reg_start, reg_len, reg) in
            self.space.covered_by((Included(offset), Included(last_position)))
        {
            let mut skip_front = 0;
            let mut split_back = 0;
            let mut reg_offset = 0;

            match position.cmp(&reg_start) {
                Ordering::Equal => {
                    if remain > reg_len {
                        split_back = remain - reg_len;
                    }
                }
                Ordering::Less => {
                    debug_assert!(position + remain > reg_start);
                    let skip = reg_start - position;

                    skip_front = skip;
                    if remain - skip > reg_len {
                        split_back = remain - (skip + reg_len);
                    }
                }
                Ordering::Greater => {
                    let offset = position - reg_start;

                    reg_offset = offset;
                    if offset + remain > reg_len {
                        split_back = offset + remain - reg_len;
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

            position += reg_offset + skip_front + xfer_len;
            consumed += skip_front + xfer_len;
            remain -= skip_front + xfer_len;
        }
    }
}
impl<ID: Copy + Eq> RegMap<ID> {
    pub fn create_packed(
        size: usize,
        regdef: &[(ID, usize)],
        resv_reg: Option<ID>,
    ) -> Self {
        let mut map = RegMap::new(size);
        let mut off = 0;
        for reg in regdef.iter() {
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
