// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::aml::{AmlBuilder, AmlWriter};
use super::names::EisaId;
use super::resources::ResourceTemplateBuilder;

const PCI_CONFIG_IO_BASE: u16 = 0x0CF8;
const PCI_CONFIG_IO_SIZE: u8 = 8;

const PS2_DATA_PORT: u16 = 0x60;
const PS2_CMD_PORT: u16 = 0x64;
const PS2_KBD_IRQ: u8 = 1;

const PCI_INT_PINS: u8 = 4;
const PCI_GSI_BASE: u32 = 16;
const PCI_SLOTS: u8 = 32;
const PRT_ENTRY_SIZE: u8 = 4;
const PCI_ADR_ALL_FUNC: u32 = 0xFFFF;

const COM_PORT_IO_LEN: u8 = 8;
const IO_ALIGN_BYTE: u8 = 1;

const QEMU_PANIC_PORT: u16 = 0x0505;

const SLP_TYP_S0: u8 = 5;
const SLP_TYP_S3: u8 = 1;
const SLP_TYP_S4: u8 = 6;
const SLP_TYP_S5: u8 = 7;

#[derive(Clone, Copy)]
pub struct ComPortConfig {
    pub io_base: u16,
    pub irq: u8,
}

#[derive(Clone, Copy)]
pub struct PcieConfig {
    pub ecam_base: u64,
    pub ecam_size: u64,
    pub bus_start: u8,
    pub bus_end: u8,
    pub mmio32_base: u64,
    pub mmio32_limit: u64,
    pub mmio64_base: u64,
    pub mmio64_limit: u64,
}

pub struct DsdtConfig {
    pub pcie: Option<PcieConfig>,
    pub com_ports: Vec<ComPortConfig>,
}

struct PrtEntry {
    slot: u8,
    pin: u8,
    gsi: u32,
}

impl AmlWriter for PrtEntry {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        let addr: u32 = ((self.slot as u32) << 16) | PCI_ADR_ALL_FUNC;

        let mut content = Vec::new();
        addr.write_aml(&mut content);
        self.pin.write_aml(&mut content);
        0u8.write_aml(&mut content);
        self.gsi.write_aml(&mut content);

        super::aml::write_package_raw(buf, PRT_ENTRY_SIZE, &content);
    }
}

pub fn build_dsdt_aml(config: &DsdtConfig) -> Vec<u8> {
    let mut builder = AmlBuilder::new();

    builder.name("PICM", &0u8);

    {
        let mut pic = builder.method("_PIC", 1, false);
        pic.store_arg_to_name(0, "PICM");
    }

    builder.name_package("\\_S0_", &[SLP_TYP_S0, SLP_TYP_S0, 0, 0]);
    builder.name_package("\\_S3_", &[SLP_TYP_S3, SLP_TYP_S3, 0, 0]);
    builder.name_package("\\_S4_", &[SLP_TYP_S4, SLP_TYP_S4, 0, 0]);
    builder.name_package("\\_S5_", &[SLP_TYP_S5, SLP_TYP_S5, 0, 0]);

    {
        let mut sb = builder.scope("\\_SB_");

        if let Some(pcie) = &config.pcie {
            build_pcie_host_bridge(&mut sb, pcie);
        }

        for (i, com) in config.com_ports.iter().enumerate() {
            build_com_port(&mut sb, i, com);
        }

        build_ps2_devices(&mut sb);
        build_qemu_panic_device(&mut sb);
    }

    builder.finish()
}

fn build_pcie_host_bridge(
    sb: &mut super::aml::ScopeGuard<'_>,
    pcie: &PcieConfig,
) {
    let mut pci0 = sb.device("PCI0");

    pci0.name("_HID", &EisaId::from_str("PNP0A08"));
    pci0.name("_CID", &EisaId::from_str("PNP0A03"));
    pci0.name("_SEG", &0u32);
    pci0.name("_UID", &0u32);
    pci0.name("_ADR", &0u32);

    let mut crs = ResourceTemplateBuilder::new();

    let bus_count = (pcie.bus_end as u16) - (pcie.bus_start as u16) + 1;
    crs.word_bus_number(
        pcie.bus_start as u16,
        pcie.bus_end as u16,
        0,
        bus_count,
    );

    crs.io(
        PCI_CONFIG_IO_BASE,
        PCI_CONFIG_IO_BASE,
        IO_ALIGN_BYTE,
        PCI_CONFIG_IO_SIZE,
    );

    crs.fixed_memory(pcie.ecam_base as u32, pcie.ecam_size as u32);

    let ecam_end = pcie.ecam_base + pcie.ecam_size;

    if pcie.ecam_base > pcie.mmio32_base {
        let len = pcie.ecam_base - pcie.mmio32_base;
        crs.dword_memory(
            false,
            true,
            pcie.mmio32_base as u32,
            (pcie.ecam_base - 1) as u32,
            0,
            len as u32,
        );
    }

    if pcie.mmio32_limit >= ecam_end {
        let len = pcie.mmio32_limit - ecam_end + 1;
        crs.dword_memory(
            false,
            true,
            ecam_end as u32,
            pcie.mmio32_limit as u32,
            0,
            len as u32,
        );
    }

    if pcie.mmio64_limit > pcie.mmio64_base {
        let len = pcie.mmio64_limit - pcie.mmio64_base + 1;
        crs.qword_memory(
            false,
            true,
            pcie.mmio64_base,
            pcie.mmio64_limit,
            0,
            len,
        );
    }

    pci0.name("_CRS", &crs);

    let mut prt_entries: Vec<PrtEntry> = Vec::new();
    for slot in 1..PCI_SLOTS {
        for pin in 0..PCI_INT_PINS {
            let gsi = PCI_GSI_BASE
                + (((slot as u32) + (pin as u32)) % (PCI_INT_PINS as u32));
            prt_entries.push(PrtEntry { slot, pin, gsi });
        }
    }
    pci0.name_package("_PRT", &prt_entries);
}

fn build_com_port(
    sb: &mut super::aml::ScopeGuard<'_>,
    index: usize,
    com: &ComPortConfig,
) {
    let name = match index {
        0 => "COM1",
        1 => "COM2",
        2 => "COM3",
        3 => "COM4",
        _ => return,
    };

    let mut dev = sb.device(name);

    dev.name("_HID", &EisaId::from_str("PNP0501"));
    dev.name("_UID", &(index as u32));

    let mut crs = ResourceTemplateBuilder::new();
    crs.io(com.io_base, com.io_base, IO_ALIGN_BYTE, COM_PORT_IO_LEN);
    crs.irq(1u16 << com.irq);
    dev.name("_CRS", &crs);
}

fn build_ps2_devices(sb: &mut super::aml::ScopeGuard<'_>) {
    let mut kbd = sb.device("KBD_");

    kbd.name("_HID", &EisaId::from_str("PNP0303"));

    let mut crs = ResourceTemplateBuilder::new();
    crs.io(PS2_DATA_PORT, PS2_DATA_PORT, 1, 1);
    crs.io(PS2_CMD_PORT, PS2_CMD_PORT, 1, 1);
    crs.irq(1u16 << PS2_KBD_IRQ);
    kbd.name("_CRS", &crs);
}

fn build_qemu_panic_device(sb: &mut super::aml::ScopeGuard<'_>) {
    let mut dev = sb.device("PEVT");

    dev.name("_HID", &"QEMU0001");

    let mut crs = ResourceTemplateBuilder::new();
    crs.io(QEMU_PANIC_PORT, QEMU_PANIC_PORT, 1, 1);
    dev.name("_CRS", &crs);

    dev.name("_STA", &0x0Fu32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let config = DsdtConfig {
            pcie: Some(PcieConfig {
                ecam_base: 0xe000_0000,
                ecam_size: 0x1000_0000,
                bus_start: 0,
                bus_end: 255,
                mmio32_base: 0xc000_0000,
                mmio32_limit: 0xfbff_ffff,
                mmio64_base: 0x1_0000_0000,
                mmio64_limit: 0xf_ffff_ffff,
            }),
            com_ports: vec![
                ComPortConfig { io_base: 0x3f8, irq: 4 },
                ComPortConfig { io_base: 0x2f8, irq: 3 },
            ],
        };
        let aml = build_dsdt_aml(&config);
        assert!(aml.windows(4).any(|w| w == b"_SB_"));
        assert!(aml.windows(4).any(|w| w == b"PCI0"));
        assert!(aml.windows(4).any(|w| w == b"COM1"));
    }
}
