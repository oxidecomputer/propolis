// PCI config registers

pub const LEN_CFG: usize = 0x100;
pub const LEN_CFG_STD: usize = 0x40;

bitflags! {
    #[derive(Default)]
    pub struct RegCmd: u16 {
    const IO_EN = 1 << 0;
    const MMIO_EN = 1 << 1;
    const BUSMSTR_EN = 1 << 2;
    const INTX_DIS = 1 << 10;
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
