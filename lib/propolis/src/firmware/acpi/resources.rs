// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI resource template encoding.
//!
//! See ACPI Specification 6.4, Section 6.4 "Resource Data Types for ACPI":
//! <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#resource-data-types-for-acpi>

use super::aml::AmlWriter;

// Table 6.27 "Small Resource Items"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#small-resource-data-type
const SMALL_IRQ_TAG: u8 = 0x04;
const SMALL_IO_TAG: u8 = 0x08;
const SMALL_END_TAG: u8 = 0x0F;

// Table 6.40 "Large Resource Items"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#large-resource-data-type
const LARGE_DWORD_ADDR_SPACE: u8 = 0x07;
const LARGE_WORD_ADDR_SPACE: u8 = 0x08;
const LARGE_EXT_IRQ: u8 = 0x09;
const LARGE_QWORD_ADDR_SPACE: u8 = 0x0A;

// Table 6.48 "QWord Address Space Descriptor"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#qword-address-space-descriptor
const ADDR_SPACE_TYPE_MEMORY: u8 = 0x00;
const ADDR_SPACE_TYPE_IO: u8 = 0x01;
const ADDR_SPACE_TYPE_BUS: u8 = 0x02;

// Table 6.49 "Memory Resource Flag (Resource Type = 0) Definitions"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#qword-address-space-descriptor
const MEM_FLAG_READ_WRITE: u8 = 1 << 0;
const MEM_FLAG_CACHEABLE: u8 = 1 << 1;
const MEM_FLAG_WRITE_COMBINING: u8 = 1 << 2;

// Table 6.56 "Extended Interrupt Descriptor Definition"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#extended-interrupt-descriptor
const EXT_IRQ_FLAG_CONSUMER: u8 = 1 << 0;
const EXT_IRQ_FLAG_EDGE: u8 = 1 << 1;
const EXT_IRQ_FLAG_ACTIVE_LOW: u8 = 1 << 2;
const EXT_IRQ_FLAG_SHARED: u8 = 1 << 3;

// Table 6.33 "I/O Port Descriptor Definition"
// https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/06_Device_Configuration/Device_Configuration.html#i-o-port-descriptor
const IO_DECODE_16BIT: u8 = 1 << 0;

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
        // 3 bytes of header + 5 u64 fields
        let data_len = (3 + 5 * std::mem::size_of::<u64>()) as u16;

        self.buf.push(0x80 | LARGE_QWORD_ADDR_SPACE);
        self.buf.extend_from_slice(&data_len.to_le_bytes());

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
        // 3 bytes of header + 5 u16 fields
        let data_len = (3 + 5 * std::mem::size_of::<u16>()) as u16;

        self.buf.push(0x80 | LARGE_WORD_ADDR_SPACE);
        self.buf.extend_from_slice(&data_len.to_le_bytes());

        self.buf.push(ADDR_SPACE_TYPE_BUS);
        self.buf.push(0x00); // General flags
        self.buf.push(0x00); // Type-specific flags

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
        // 3 bytes of header + 5 u32 fields
        let data_len = (3 + 5 * std::mem::size_of::<u32>()) as u16;

        self.buf.push(0x80 | LARGE_DWORD_ADDR_SPACE);
        self.buf.extend_from_slice(&data_len.to_le_bytes());

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
        // info(1) + min(2) + max(2) + align(1) + len(1)
        let data_len = 1 + 2 * std::mem::size_of::<u16>() + 2;

        self.buf.push((SMALL_IO_TAG << 3) | data_len as u8);
        self.buf.push(IO_DECODE_16BIT);
        self.buf.extend_from_slice(&min.to_le_bytes());
        self.buf.extend_from_slice(&max.to_le_bytes());
        self.buf.push(align);
        self.buf.push(len);
        self
    }

    pub fn irq(&mut self, irq_mask: u16) -> &mut Self {
        let data_len = std::mem::size_of::<u16>();

        self.buf.push((SMALL_IRQ_TAG << 3) | data_len as u8);
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

        self.buf.push(0x80 | LARGE_EXT_IRQ);
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
        self.buf.push((SMALL_END_TAG << 3) | 1); // 1 byte checksum
        self.buf.push(0x00);
        self.buf
    }
}

impl AmlWriter for ResourceTemplateBuilder {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        let mut data = Vec::with_capacity(self.buf.len() + 2);
        data.extend_from_slice(&self.buf);
        data.push((SMALL_END_TAG << 3) | 1);
        data.push(0x00);
        data.as_slice().write_aml(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_descriptors() {
        let io_data_len = 1 + 2 * std::mem::size_of::<u16>() + 2;
        let irq_data_len = std::mem::size_of::<u16>();

        let mut builder = ResourceTemplateBuilder::new();
        builder.io(0x3F8, 0x3F8, 1, 8);
        let data = builder.finish();
        assert_eq!(data[0], (SMALL_IO_TAG << 3) | io_data_len as u8);
        assert_eq!(data[1], IO_DECODE_16BIT);

        let mut builder = ResourceTemplateBuilder::new();
        builder.irq(0x0010);
        let data = builder.finish();
        assert_eq!(data[0], (SMALL_IRQ_TAG << 3) | irq_data_len as u8);
    }

    #[test]
    fn large_descriptors() {
        let mut builder = ResourceTemplateBuilder::new();
        builder.word_bus_number(0, 255, 0, 256);
        let data = builder.finish();
        assert_eq!(data[0], 0x80 | LARGE_WORD_ADDR_SPACE);
        assert_eq!(data[3], ADDR_SPACE_TYPE_BUS);

        let mut builder = ResourceTemplateBuilder::new();
        builder.qword_memory(
            false,
            true,
            0xE000_0000,
            0xEFFF_FFFF,
            0,
            0x1000_0000,
        );
        let data = builder.finish();
        assert_eq!(data[0], 0x80 | LARGE_QWORD_ADDR_SPACE);
        assert_eq!(data[3], ADDR_SPACE_TYPE_MEMORY);
    }

    #[test]
    fn chained_resources() {
        let mut builder = ResourceTemplateBuilder::new();
        builder
            .word_bus_number(0, 255, 0, 256)
            .io(0xCF8, 0xCFF, 1, 8)
            .qword_memory(
                false,
                true,
                0xE000_0000,
                0xEFFF_FFFF,
                0,
                0x1000_0000,
            );
        let data = builder.finish();

        let len = data.len();
        assert_eq!(data[len - 2], (SMALL_END_TAG << 3) | 1);
    }
}
