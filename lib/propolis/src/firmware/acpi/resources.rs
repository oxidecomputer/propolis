// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI resource template encoding (see ACPI spec section 6.4).

use super::aml::AmlWriter;

const SMALL_IRQ_TAG: u8 = 0x04;
const SMALL_IO_TAG: u8 = 0x08;
const SMALL_END_TAG: u8 = 0x0F;

const LARGE_RESOURCE_BIT: u8 = 0x80;

const LARGE_QWORD_ADDR_SPACE: u8 = 0x0A;
const LARGE_DWORD_ADDR_SPACE: u8 = 0x07;
const LARGE_WORD_ADDR_SPACE: u8 = 0x08;
const LARGE_MEMORY32_FIXED: u8 = 0x06;
const LARGE_EXT_IRQ: u8 = 0x09;

const ADDR_SPACE_TYPE_MEMORY: u8 = 0x00;
const ADDR_SPACE_TYPE_IO: u8 = 0x01;
const ADDR_SPACE_TYPE_BUS: u8 = 0x02;

const MEM_FLAG_READ_WRITE: u8 = 0x01;
const MEM_FLAG_CACHEABLE: u8 = 0x02;
const MEM_FLAG_WRITE_COMBINING: u8 = 0x04;

const EXT_IRQ_FLAG_CONSUMER: u8 = 0x01;
const EXT_IRQ_FLAG_EDGE: u8 = 0x02;
const EXT_IRQ_FLAG_ACTIVE_LOW: u8 = 0x04;
const EXT_IRQ_FLAG_SHARED: u8 = 0x08;

const IO_DECODE_16BIT: u8 = 0x01;

fn mem_type_flags(cacheable: bool, read_write: bool) -> u8 {
    let mut f = 0u8;
    if cacheable {
        f |= MEM_FLAG_CACHEABLE | MEM_FLAG_WRITE_COMBINING;
    }
    if read_write {
        f |= MEM_FLAG_READ_WRITE;
    }
    f
}

const QWORD_ADDR_SPACE_DATA_LEN: u16 = 43;
const WORD_ADDR_SPACE_DATA_LEN: u16 = 13;
const DWORD_ADDR_SPACE_DATA_LEN: u16 = 23;
const FIXED_MEMORY32_DATA_LEN: u16 = 9;

const ADDR_SPACE_FLAG_MIF: u8 = 0x04;
const ADDR_SPACE_FLAG_MAF: u8 = 0x08;

const IO_RANGE_ENTIRE: u8 = 0x03;

const SMALL_IO_LEN: u8 = 0x07;
const SMALL_IRQ_LEN: u8 = 0x02;
const SMALL_END_LEN: u8 = 0x01;

/// Builder for ACPI resource templates used in _CRS, _PRS and _SRS methods.
#[must_use = "call .finish() to get the resource template bytes"]
pub struct ResourceTemplateBuilder {
    buf: Vec<u8>,
}

impl ResourceTemplateBuilder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn qword_memory(
        &mut self,
        cacheable: bool,
        read_write: bool,
        min: u64,
        max: u64,
        translation: u64,
        len: u64,
    ) -> &mut Self {
        self.qword_address_space(
            ADDR_SPACE_TYPE_MEMORY,
            cacheable,
            read_write,
            min,
            max,
            translation,
            len,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn qword_address_space(
        &mut self,
        resource_type: u8,
        cacheable: bool,
        read_write: bool,
        min: u64,
        max: u64,
        translation: u64,
        len: u64,
    ) -> &mut Self {
        self.buf.push(LARGE_RESOURCE_BIT | LARGE_QWORD_ADDR_SPACE);
        self.buf.extend_from_slice(&QWORD_ADDR_SPACE_DATA_LEN.to_le_bytes());

        self.buf.push(resource_type);
        self.buf.push(0x00); // General flags

        let type_flags = if resource_type == ADDR_SPACE_TYPE_MEMORY {
            mem_type_flags(cacheable, read_write)
        } else {
            0x00
        };
        self.buf.push(type_flags);

        self.buf.extend_from_slice(&0u64.to_le_bytes()); // Granularity
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.extend_from_slice(&translation.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());

        self
    }

    pub fn word_bus_number(
        &mut self,
        min: u16,
        max: u16,
        translation: u16,
        len: u16,
    ) -> &mut Self {
        self.buf.push(LARGE_RESOURCE_BIT | LARGE_WORD_ADDR_SPACE);
        self.buf.extend_from_slice(&WORD_ADDR_SPACE_DATA_LEN.to_le_bytes());

        self.buf.push(ADDR_SPACE_TYPE_BUS);
        self.buf.push(0x00); // General flags
        self.buf.push(0x00); // Type specific flags

        self.buf.extend_from_slice(&0u16.to_le_bytes()); // Granularity
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.extend_from_slice(&translation.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());

        self
    }

    pub fn dword_memory(
        &mut self,
        cacheable: bool,
        read_write: bool,
        min: u32,
        max: u32,
        translation: u32,
        len: u32,
    ) -> &mut Self {
        self.buf.push(LARGE_RESOURCE_BIT | LARGE_DWORD_ADDR_SPACE);
        self.buf.extend_from_slice(&DWORD_ADDR_SPACE_DATA_LEN.to_le_bytes());

        self.buf.push(ADDR_SPACE_TYPE_MEMORY);
        self.buf.push(0x00); // General flags
        self.buf.push(mem_type_flags(cacheable, read_write));

        self.buf.extend_from_slice(&0u32.to_le_bytes()); // Granularity
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.extend_from_slice(&translation.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());

        self
    }

    pub fn io(&mut self, min: u16, max: u16, align: u8, len: u8) -> &mut Self {
        self.buf.push((SMALL_IO_TAG << 3) | SMALL_IO_LEN);
        self.buf.push(IO_DECODE_16BIT);
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.push(align);
        self.buf.push(len);
        self
    }

    pub fn io_range(&mut self, min: u16, max: u16, len: u16) -> &mut Self {
        self.buf.push(LARGE_RESOURCE_BIT | LARGE_WORD_ADDR_SPACE);
        self.buf.extend_from_slice(&WORD_ADDR_SPACE_DATA_LEN.to_le_bytes());

        self.buf.push(ADDR_SPACE_TYPE_IO);
        self.buf.push(ADDR_SPACE_FLAG_MIF | ADDR_SPACE_FLAG_MAF);
        self.buf.push(IO_RANGE_ENTIRE);

        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());

        self
    }

    pub fn fixed_memory(&mut self, base: u32, len: u32) -> &mut Self {
        self.buf.push(LARGE_RESOURCE_BIT | LARGE_MEMORY32_FIXED);
        self.buf.extend_from_slice(&FIXED_MEMORY32_DATA_LEN.to_le_bytes());
        self.buf.push(MEM_FLAG_READ_WRITE);
        self.buf.extend_from_slice(&base.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());
        self
    }

    pub fn irq(&mut self, irq_mask: u16) -> &mut Self {
        self.buf.push((SMALL_IRQ_TAG << 3) | SMALL_IRQ_LEN);
        self.buf.extend_from_slice(&irq_mask.to_le_bytes());
        self
    }

    pub fn extended_irq(
        &mut self,
        consumer: bool,
        edge_triggered: bool,
        active_low: bool,
        shared: bool,
        irqs: &[u32],
    ) -> &mut Self {
        let data_len = 2 + (irqs.len() * 4);

        self.buf.push(LARGE_RESOURCE_BIT | LARGE_EXT_IRQ);
        self.buf.extend_from_slice(&(data_len as u16).to_le_bytes());

        let mut flags = 0u8;
        if consumer {
            flags |= EXT_IRQ_FLAG_CONSUMER;
        }
        if edge_triggered {
            flags |= EXT_IRQ_FLAG_EDGE;
        }
        if active_low {
            flags |= EXT_IRQ_FLAG_ACTIVE_LOW;
        }
        if shared {
            flags |= EXT_IRQ_FLAG_SHARED;
        }
        self.buf.push(flags);
        self.buf.push(irqs.len() as u8);

        for &irq in irqs {
            self.buf.extend_from_slice(&irq.to_le_bytes());
        }

        self
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push((SMALL_END_TAG << 3) | SMALL_END_LEN);
        self.buf.push(0x00); // Checksum (ignored)
        self.buf
    }
}

impl AmlWriter for ResourceTemplateBuilder {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        let mut data = Vec::with_capacity(self.buf.len() + 2);
        data.extend_from_slice(&self.buf);
        data.push((SMALL_END_TAG << 3) | SMALL_END_LEN);
        data.push(0x00);
        data.as_slice().write_aml(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_descriptors() {
        let mut builder = ResourceTemplateBuilder::new();
        builder.io(0x3F8, 0x3F8, 1, 8);
        let data = builder.finish();
        assert_eq!(data[0], (SMALL_IO_TAG << 3) | SMALL_IO_LEN);
        assert_eq!(data[1], IO_DECODE_16BIT);

        let mut builder = ResourceTemplateBuilder::new();
        builder.irq(0x0010);
        let data = builder.finish();
        assert_eq!(data[0], (SMALL_IRQ_TAG << 3) | SMALL_IRQ_LEN);
    }

    #[test]
    fn large_descriptors() {
        let mut builder = ResourceTemplateBuilder::new();
        builder.word_bus_number(0, 255, 0, 256);
        let data = builder.finish();
        assert_eq!(data[0], LARGE_RESOURCE_BIT | LARGE_WORD_ADDR_SPACE);
        assert_eq!(data[3], ADDR_SPACE_TYPE_BUS);

        let mut builder = ResourceTemplateBuilder::new();
        builder.qword_memory(false, true, 0xE000_0000, 0xEFFF_FFFF, 0, 0x1000_0000);
        let data = builder.finish();
        assert_eq!(data[0], LARGE_RESOURCE_BIT | LARGE_QWORD_ADDR_SPACE);
        assert_eq!(data[3], ADDR_SPACE_TYPE_MEMORY);
    }

    #[test]
    fn chained_resources() {
        let mut builder = ResourceTemplateBuilder::new();
        builder
            .word_bus_number(0, 255, 0, 256)
            .io(0xCF8, 0xCFF, 1, 8)
            .qword_memory(false, true, 0xE000_0000, 0xEFFF_FFFF, 0, 0x1000_0000);
        let data = builder.finish();

        let len = data.len();
        assert_eq!(data[len - 2], (SMALL_END_TAG << 3) | SMALL_END_LEN);
    }
}
