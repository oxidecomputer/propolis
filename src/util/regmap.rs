use std::ops::Bound::Included;

use super::aspace::ASpace;
use crate::types::*;

type ReadFunc<ID, OBJ> = fn(obj: &OBJ, id: &ID, ro: &mut ReadOp);
type WriteFunc<ID, OBJ> = fn(obj: &OBJ, id: &ID, wo: &WriteOp);

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

    pub fn with_ctx<'b, OBJ>(
        &self,
        obj: &'b OBJ,
        read_handler: ReadFunc<ID, OBJ>,
        write_handler: WriteFunc<ID, OBJ>,
    ) -> TransferCtx<'_, 'b, ID, OBJ> {
        TransferCtx { map: &self, obj, read_handler, write_handler }
    }

    pub fn read<OBJ>(&self, ro: &mut ReadOp, obj: &OBJ, f: ReadFunc<ID, OBJ>) {
        assert!(ro.buf.len() != 0);
        assert!(ro.offset + ro.buf.len() - 1 < self.len);

        let buf_len = ro.buf.len();
        let buf = &mut ro.buf;
        self.iterate_transfers(ro.offset, buf_len, |xfer: &RegXfer<ID>| {
            let buf_xfer = &mut buf[xfer.skip_front_idx..xfer.split_back_idx];
            let mut copy_op = ReadOp::new(xfer.offset, buf_xfer);

            debug_assert!(copy_op.buf.len() != 0);
            Self::reg_read(xfer.reg, xfer.reg_len, &mut copy_op, obj, f);
        })
    }

    pub fn write<OBJ>(
        &self,
        wo: &WriteOp,
        obj: &OBJ,
        wf: WriteFunc<ID, OBJ>,
        rf: ReadFunc<ID, OBJ>,
    ) {
        assert!(wo.buf.len() != 0);
        assert!(wo.offset + wo.buf.len() - 1 < self.len);

        let buf_len = wo.buf.len();
        let buf = wo.buf;
        self.iterate_transfers(wo.offset, buf_len, |xfer| {
            let buf_xfer = &buf[xfer.skip_front_idx..xfer.split_back_idx];
            let copy_op = WriteOp::new(xfer.offset, buf_xfer);

            debug_assert!(buf_xfer.len() != 0);
            Self::reg_write(xfer.reg, xfer.reg_len, &copy_op, obj, wf, rf);
        })
    }

    fn reg_read<OBJ>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &mut ReadOp,
        obj: &OBJ,
        read_handler: ReadFunc<ID, OBJ>,
    ) {
        if reg.flags.contains(Flags::NO_READ_EXTEND)
            && reg_len != copy_op.buf.len()
        {
            read_handler(obj, &reg.id, copy_op);
        } else if reg_len == copy_op.buf.len() {
            debug_assert!(copy_op.offset == 0);
            read_handler(obj, &reg.id, copy_op);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            read_handler(obj, &reg.id, &mut ReadOp::new(0, &mut scratch));
            copy_op.buf.copy_from_slice(
                &scratch[copy_op.offset..(copy_op.offset + copy_op.buf.len())],
            );
        }
    }

    fn reg_write<OBJ>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_op: &WriteOp,
        obj: &OBJ,
        write_handler: WriteFunc<ID, OBJ>,
        read_handler: ReadFunc<ID, OBJ>,
    ) {
        if reg.flags.contains(Flags::NO_WRITE_EXTEND)
            && reg_len != copy_op.buf.len()
        {
            write_handler(obj, &reg.id, copy_op);
        } else if reg_len == copy_op.buf.len() {
            debug_assert!(copy_op.offset == 0);
            write_handler(obj, &reg.id, copy_op);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            if !reg.flags.contains(Flags::NO_READ_MOD_WRITE) {
                read_handler(obj, &reg.id, &mut ReadOp::new(0, &mut scratch));
            }
            &mut scratch[copy_op.offset..(copy_op.offset + copy_op.buf.len())]
                .copy_from_slice(copy_op.buf);

            write_handler(obj, &reg.id, &WriteOp::new(0, &scratch));
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

            if position == reg_start {
                if remain > reg_len {
                    split_back = remain - reg_len;
                }
            } else if position < reg_start {
                debug_assert!(position + remain > reg_start);
                let skip = reg_start - position;

                skip_front = skip;
                if remain - skip > reg_len {
                    split_back = remain - (skip + reg_len);
                }
            } else {
                // position > reg_start
                let offset = position - reg_start;

                reg_offset = offset;
                if offset + remain > reg_len {
                    split_back = offset + remain - reg_len;
                }
            }

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

pub struct TransferCtx<'a, 'b, ID, OBJ> {
    map: &'a RegMap<ID>,
    obj: &'b OBJ,
    read_handler: ReadFunc<ID, OBJ>,
    write_handler: WriteFunc<ID, OBJ>,
}

impl<'a, 'b, ID, OBJ> TransferCtx<'a, 'b, ID, OBJ> {
    pub fn read(&self, ro: &mut ReadOp) {
        self.map.read(ro, self.obj, self.read_handler)
    }
    pub fn write(&self, wo: &WriteOp) {
        self.map.write(wo, self.obj, self.write_handler, self.read_handler)
    }
}
