//! PCI config registers.

#![allow(unused)]

pub const LEN_CFG: usize = 0x100;
pub const LEN_CFG_STD: usize = 0x40;
pub const LEN_CFG_ECAM: usize = 0x1000;

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

/// The minimum number of buses a single ECAM region can address. The PCIe spec
/// requires that at least one bit of the ECAM address space be used to specify
/// a bus number (see PCIe base spec rev 5.0 table 7-1).
pub const PCIE_MIN_BUSES_PER_ECAM_REGION: u16 = 2;

/// The maximum number of buses a single ECAM region can address.
pub const PCIE_MAX_BUSES_PER_ECAM_REGION: u16 = 256;

/// Bitwise AND'ing an ECAM MMIO access address with this mask produces an
/// offset in bytes at which to access the target BDF's configuration region.
pub const MASK_ECAM_CFG_OFFSET: usize = 0xfff;
