use std::ops::Bound::Included;

use super::aspace::ASpace;

type ReadFunc<ID, BE> = fn(be: &BE, id: &ID, foff: usize, outb: &mut [u8]);
type WriteFunc<ID, BE> = fn(be: &BE, id: &ID, foff: usize, inb: &[u8]);

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
    skip_front: usize,
    split_back: usize,
}

impl<ID> RegMap<ID> {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            space: ASpace::new(0, len - 1),
        }
    }

    pub fn define(&mut self, start: usize, len: usize, id: ID) {
        self.define_with_flags(start, len, id, Flags::DEFAULT)
    }

    pub fn define_with_flags(&mut self, start: usize, len: usize, id: ID, flags: Flags) {
        self.space
            .register(start, len, RegDef { id, flags })
            .unwrap();
    }

    pub fn in_ctx<'b, BE>(
        &self,
        read_handler: ReadFunc<ID, BE>,
        write_handler: WriteFunc<ID, BE>,
        be: &'b BE,
    ) -> TransferCtx<'_, 'b, ID, BE> {
        TransferCtx {
            map: &self,
            be,
            read_handler,
            write_handler,
        }
    }

    pub fn read<BE>(&self, offset: usize, buf: &mut [u8], be: &BE, f: ReadFunc<ID, BE>) {
        assert!(buf.len() != 0);
        assert!(offset + buf.len() - 1 < self.len);

        let buf_len = buf.len();
        self.iterate_transfers(offset, buf_len, |xfer: &RegXfer<ID>| {
            let buf_xfer = &mut buf[xfer.skip_front..xfer.split_back];

            debug_assert!(buf_xfer.len() != 0);
            Self::reg_read(xfer.reg, xfer.reg_len, xfer.offset, buf_xfer, be, f);
        })
    }

    pub fn write<BE>(
        &self,
        offset: usize,
        buf: &[u8],
        be: &BE,
        wf: WriteFunc<ID, BE>,
        rf: ReadFunc<ID, BE>,
    ) {
        assert!(buf.len() != 0);
        assert!(offset + buf.len() - 1 < self.len);

        let buf_len = buf.len();
        self.iterate_transfers(offset, buf_len, |xfer| {
            let buf_xfer = &buf[xfer.skip_front..xfer.split_back];

            debug_assert!(buf_xfer.len() != 0);
            Self::reg_write(xfer.reg, xfer.reg_len, xfer.offset, buf_xfer, be, wf, rf);
        })
    }

    fn reg_read<BE>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_off: usize,
        copy_buf: &mut [u8],
        be: &BE,
        read_handler: ReadFunc<ID, BE>,
    ) {
        if reg.flags.contains(Flags::NO_READ_EXTEND) && reg_len != copy_buf.len() {
            read_handler(be, &reg.id, copy_off, copy_buf);
        } else if reg_len == copy_buf.len() {
            read_handler(be, &reg.id, 0, copy_buf);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            read_handler(be, &reg.id, 0, &mut scratch);
            copy_buf.copy_from_slice(&scratch[copy_off..(copy_off + copy_buf.len())]);
        }
    }

    fn reg_write<BE>(
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_off: usize,
        copy_buf: &[u8],
        be: &BE,
        write_handler: WriteFunc<ID, BE>,
        read_handler: ReadFunc<ID, BE>,
    ) {
        if reg.flags.contains(Flags::NO_WRITE_EXTEND) && reg_len != copy_buf.len() {
            write_handler(be, &reg.id, copy_off, copy_buf);
        } else if reg_len == copy_buf.len() {
            write_handler(be, &reg.id, 0, copy_buf);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            if !reg.flags.contains(Flags::NO_READ_MOD_WRITE) {
                read_handler(be, &reg.id, 0, &mut scratch);
            }
            &mut scratch[copy_off..(copy_off + copy_buf.len())].copy_from_slice(copy_buf);

            write_handler(be, &reg.id, 0, &scratch);
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

        for (reg_start, reg_len, reg) in self
            .space
            .covered_by((Included(offset), Included(last_position)))
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
                    split_back = skip + reg_len;
                }
            } else {
                // position > reg_start
                let offset = position - reg_start;

                reg_offset = offset;
                if offset + remain > reg_len {
                    split_back = reg_len - offset;
                }
            }

            let xfer_len = remain - (skip_front + split_back);
            debug_assert!(xfer_len <= reg_len);

            do_xfer(&RegXfer {
                reg,
                reg_len,
                offset: reg_offset,
                skip_front: consumed + skip_front,
                split_back: consumed + split_back,
            });

            position += reg_offset + skip_front + xfer_len;
            consumed += skip_front + xfer_len;
            remain -= skip_front + xfer_len;
        }
    }
}

pub struct TransferCtx<'a, 'b, ID, BE> {
    map: &'a RegMap<ID>,
    be: &'b BE,
    read_handler: ReadFunc<ID, BE>,
    write_handler: WriteFunc<ID, BE>,
}

impl<'a, 'b, ID, BE> TransferCtx<'a, 'b, ID, BE> {
    pub fn read(&self, offset: usize, outb: &mut [u8]) {
        self.map.read(offset, outb, self.be, self.read_handler)
    }
    pub fn write(&self, offset: usize, inb: &[u8]) {
        self.map
            .write(offset, inb, self.be, self.write_handler, self.read_handler)
    }
}
