//! PCI config registers.

#![allow(unused)]

pub const LEN_CFG: usize = 0x100;
pub const LEN_CFG_STD: usize = 0x40;

bitflags! {
    pub struct RegCmd: u16 {
    const IO_EN = 1 << 0;
    const MMIO_EN = 1 << 1;
    const BUSMSTR_EN = 1 << 2;
    const INTX_DIS = 1 << 10;
    }
}
impl RegCmd {
    pub fn reset(&mut self) {
        *self = RegCmd::default()
    }
}
impl Default for RegCmd {
    fn default() -> Self {
        RegCmd::INTX_DIS
    }
}

bitflags! {
    #[derive(Default)]
    pub struct RegStatus: u16 {
        const INTR_STATUS = 1 << 3;
        const CAP_LIST = 1 << 4;
    }
}

pub const BAR_TYPE_IO: u32 = 0b01;
pub const BAR_TYPE_MEM: u32 = 0b000;
pub const BAR_TYPE_MEM64: u32 = 0b100;

pub const CAP_ID_MSI: u8 = 0x05;
pub const CAP_ID_VENDOR: u8 = 0x09;
pub const CAP_ID_MSIX: u8 = 0x11;

pub const CLASS_UNCLASSIFIED: u8 = 0;
pub const CLASS_STORAGE: u8 = 1;
pub const CLASS_NETWORK: u8 = 2;
pub const CLASS_DISPLAY: u8 = 3;
pub const CLASS_MULTIMEDIA: u8 = 4;
pub const CLASS_MEMORY: u8 = 5;
pub const CLASS_BRIDGE: u8 = 6;

pub const HEADER_TYPE_DEVICE: u8 = 0b0;
pub const HEADER_TYPE_BRIDGE: u8 = 0b1;
pub const HEADER_TYPE_MULTIFUNC: u8 = 0b1000_0000;

pub const SUBCLASS_NVM: u8 = 8;

pub const PROGIF_ENTERPRISE_NVME: u8 = 2;

pub(super) const MASK_FUNC: u8 = 0x07;
pub(super) const MASK_DEV: u8 = 0x1f;
pub(super) const MASK_BUS: u8 = 0xff;

pub const PORT_PCI_CONFIG_ADDR: u16 = 0xcf8;
pub const LEN_PCI_CONFIG_ADDR: u16 = 4;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;
pub const LEN_PCI_CONFIG_DATA: u16 = 4;

/// The assumed base address for the PCIe extended configuration access
/// mechanism (ECAM)'s MMIO region.
pub const ADDR_ECAM_REGION_BASE: usize = 0xe000_0000;

/// The size of the ECAM MMIO region in bytes.
pub const LEN_ECAM_REGION: usize = 0x1000_0000;

/// Bitwise AND'ing an ECAM MMIO access address with this mask produces an
/// offset in bytes at which to access the target BDF's configuration region.
pub const MASK_ECAM_CFG_OFFSET: usize = 0xfff;

/// The word granularity for ECAM MMIO accesses. PCIe root complexes are not
/// required to handle accesses that span multiple words of this size (PCIe
/// base spec rev 5.0 SS7.2.2).
pub const LEN_ECAM_ACCESS_GRANULARITY: usize = 4;

/// Bitwise AND'ing an ECAM MMIO access address with this mask aligns it to
/// the next lower ECAM word boundary.
pub const MASK_ECAM_ACCESS_ALIGN: usize = !(LEN_ECAM_ACCESS_GRANULARITY - 1);
