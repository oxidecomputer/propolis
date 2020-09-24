use std::ops::Bound::Included;

use super::aspace::ASpace;

type ReadFunc<ID, CTX> = fn(ctx: &mut CTX, id: ID, buf: &mut [u8]);
type WriteFunc<ID, CTX> = fn(ctx: &mut CTX, id: ID, buf: &[u8]);

type PartialReadFunc<ID, CTX> = fn(ctx: &mut CTX, id: ID, off: usize, buf: &mut [u8]);
type PartialWriteFunc<ID, CTX> = fn(ctx: &mut CTX, id: ID, off: usize, buf: &[u8]);

struct RegDef<ID>
where
    ID: Clone + Copy,
{
    id: ID,
    flags: Flags,
}

pub struct RegMap<ID, CTX>
where
    ID: Clone + Copy,
{
    len: usize,
    space: ASpace<RegDef<ID>>,
    read_handler: ReadFunc<ID, CTX>,
    write_handler: WriteFunc<ID, CTX>,
    partial_read_handler: Option<PartialReadFunc<ID, CTX>>,
    partial_write_handler: Option<PartialWriteFunc<ID, CTX>>,
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

impl<ID, CTX> RegMap<ID, CTX>
where
    ID: Clone + Copy,
{
    pub fn new(
        len: usize,
        read_handler: ReadFunc<ID, CTX>,
        write_handler: WriteFunc<ID, CTX>,
    ) -> Self {
        Self {
            len,
            space: ASpace::new(0, len - 1),
            read_handler,
            write_handler,
            partial_read_handler: None,
            partial_write_handler: None,
        }
    }

    pub fn define(&mut self, start: usize, len: usize, id: ID) {
        self.define_with_flags(start, len, id, Flags::DEFAULT)
    }

    pub fn define_with_flags(&mut self, start: usize, len: usize, id: ID, flags: Flags) {
        // XXX do proper error handling
        if flags.contains(Flags::NO_READ_EXTEND) {
            assert!(self.partial_read_handler.is_some());
        }
        if flags.contains(Flags::NO_WRITE_EXTEND) {
            assert!(self.partial_write_handler.is_some());
        }

        self.space
            .register(start, len, RegDef { id, flags })
            .unwrap();
    }

    pub fn read(&self, offset: usize, buf: &mut [u8], ctx: &mut CTX) {
        assert!(buf.len() != 0);
        assert!(offset + buf.len() - 1 < self.len);

        let last_position = offset + buf.len() - 1;
        let mut position = offset;
        let mut active_buf = buf;

        for (reg_start, reg_len, reg) in self
            .space
            .covered_by((Included(offset), Included(last_position)))
        {
            match Coverage::calculate(reg_start, reg_len, position, active_buf.len()) {
                Coverage::Full => {
                    debug_assert!(position == reg_start);

                    let (copy_buf, remain) = active_buf.split_at_mut(reg_len);
                    self.do_read(reg, reg_len, 0, copy_buf, ctx);
                    position += reg_len;
                    active_buf = remain;
                }
                Coverage::OverlapFront => {
                    debug_assert!(position == reg_start);

                    self.do_read(reg, reg_len, 0, active_buf, ctx);
                    position += active_buf.len();
                    active_buf = &mut [];
                }
                Coverage::OverlapBack(split) => {
                    debug_assert!(position > reg_start);

                    let (copy_buf, remain) = active_buf.split_at_mut(split);
                    self.do_read(reg, reg_len, position - reg_start, copy_buf, ctx);
                    position += copy_buf.len();
                    active_buf = remain;
                }
                Coverage::Middling => {
                    debug_assert!(position != reg_start);

                    self.do_read(reg, reg_len, position - reg_start, active_buf, ctx);
                    position += active_buf.len();
                    active_buf = &mut [];
                }
            }
        }
        debug_assert!(position - 1 == last_position);
    }

    pub fn write(&self, offset: usize, buf: &[u8], ctx: &mut CTX) {
        assert!(buf.len() != 0);
        assert!(offset + buf.len() < self.len);

        let last_position = offset + buf.len() - 1;
        let mut position = offset;
        let mut active_buf = buf;

        for (reg_start, reg_len, reg) in self
            .space
            .covered_by((Included(offset), Included(last_position)))
        {
            match Coverage::calculate(reg_start, reg_len, position, active_buf.len()) {
                Coverage::Full => {
                    debug_assert!(position == reg_start);
                    let (copy_buf, remain) = active_buf.split_at(reg_len);
                    self.do_write(reg, reg_len, 0, copy_buf, ctx);
                    position += reg_len;
                    active_buf = remain;
                }
                Coverage::OverlapFront => {
                    debug_assert!(position == reg_start);
                    self.do_write(reg, reg_len, 0, active_buf, ctx);
                    position += active_buf.len();
                    active_buf = &mut [];
                }
                Coverage::OverlapBack(split) => {
                    debug_assert!(position > reg_start);
                    let (copy_buf, remain) = active_buf.split_at(split);
                    self.do_write(reg, reg_len, position - reg_start, copy_buf, ctx);
                    position += copy_buf.len();
                    active_buf = remain;
                }
                Coverage::Middling => {
                    debug_assert!(position != reg_start);
                    self.do_write(reg, reg_len, position - reg_start, active_buf, ctx);
                    position += active_buf.len();
                    active_buf = &mut [];
                }
            }
        }
        debug_assert!(position - 1 == last_position);
    }

    fn do_read(
        &self,
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_off: usize,
        copy_buf: &mut [u8],
        ctx: &mut CTX,
    ) {
        let read_handler = self.read_handler;
        if reg.flags.contains(Flags::NO_READ_EXTEND) && reg_len != copy_buf.len() {
            let partial_read = self.partial_read_handler.unwrap();
            partial_read(ctx, reg.id, copy_off, copy_buf);
        } else if reg_len == copy_buf.len() {
            read_handler(ctx, reg.id, copy_buf);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            read_handler(ctx, reg.id, &mut scratch);
            copy_buf.copy_from_slice(&scratch[copy_off..(copy_off + copy_buf.len())]);
        }
    }

    fn do_write(
        &self,
        reg: &RegDef<ID>,
        reg_len: usize,
        copy_off: usize,
        copy_buf: &[u8],
        ctx: &mut CTX,
    ) {
        let write_handler = self.write_handler;
        if reg.flags.contains(Flags::NO_WRITE_EXTEND) && reg_len != copy_buf.len() {
            let partial_write = self.partial_write_handler.unwrap();
            partial_write(ctx, reg.id, copy_off, copy_buf);
        } else if reg_len == copy_buf.len() {
            write_handler(ctx, reg.id, copy_buf);
        } else {
            let mut scratch = Vec::new();
            scratch.resize(reg_len, 0);

            debug_assert!(scratch.len() == reg_len);
            if !reg.flags.contains(Flags::NO_READ_MOD_WRITE) {
                let read_handler = self.read_handler;
                read_handler(ctx, reg.id, &mut scratch);
            }
            &mut scratch[copy_off..(copy_off + copy_buf.len())].copy_from_slice(copy_buf);

            write_handler(ctx, reg.id, &scratch);
        }
    }
}

enum Coverage {
    /// Buffer covers entire register (and possibly beyond)
    Full,
    /// Buffer starts at front of register but falls short of its end
    OverlapFront,
    /// Buffer begins in middle of register and extends to (or past) the end
    OverlapBack(usize),
    /// Buffer shorter than register and contained within
    Middling,
}

impl Coverage {
    fn calculate(reg_start: usize, reg_len: usize, buf_start: usize, buf_len: usize) -> Self {
        debug_assert!(buf_start >= reg_start);

        if buf_start == reg_start {
            if buf_len >= reg_len {
                Coverage::Full
            } else {
                Coverage::OverlapFront
            }
        } else {
            let offset = buf_start - reg_start;

            if offset + buf_len >= reg_len {
                Coverage::OverlapBack(reg_len - offset)
            } else {
                Coverage::Middling
            }
        }
    }
}
