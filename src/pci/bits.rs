// PCI config registers
pub const OFF_CFG_VENDORID: u8 = 0x00;
pub const OFF_CFG_DEVICEID: u8 = 0x02;
pub const OFF_CFG_COMMAND: u8 = 0x04;
pub const OFF_CFG_STATUS: u8 = 0x06;
pub const OFF_CFG_REVISIONID: u8 = 0x08;
pub const OFF_CFG_PROGIF: u8 = 0x09;
pub const OFF_CFG_SUBCLASS: u8 = 0x0a;
pub const OFF_CFG_CLASS: u8 = 0x0b;
pub const OFF_CFG_CACHELINESZ: u8 = 0x0c;
pub const OFF_CFG_LATENCYTIMER: u8 = 0x0d;
pub const OFF_CFG_HEADERTYPE: u8 = 0x0e;
pub const OFF_CFG_BIST: u8 = 0x0f;
pub const OFF_CFG_BAR0: u8 = 0x10;
pub const OFF_CFG_BAR1: u8 = 0x14;
pub const OFF_CFG_BAR2: u8 = 0x18;
pub const OFF_CFG_BAR3: u8 = 0x1c;
pub const OFF_CFG_BAR4: u8 = 0x20;
pub const OFF_CFG_BAR5: u8 = 0x24;
pub const OFF_CFG_CARDBUSPTR: u8 = 0x28;
pub const OFF_CFG_SUBVENDORID: u8 = 0x2c;
pub const OFF_CFG_SUBDEVICEID: u8 = 0x2e;
pub const OFF_CFG_EXPROMADDR: u8 = 0x30;
pub const OFF_CFG_CAPPTR: u8 = 0x34;
pub const OFF_CFG_RESERVED: u8 = 0x35;
pub const OFF_CFG_INTRLINE: u8 = 0x3c;
pub const OFF_CFG_INTRPIN: u8 = 0x3d;
pub const OFF_CFG_MINGRANT: u8 = 0x3e;
pub const OFF_CFG_MAXLATENCY: u8 = 0x3f;

pub const LEN_CFG: usize = 0x100;
pub const LEN_CFG_STD: usize = 0x40;

pub const REG_CMD_IO_EN: u16 = 0b1;
pub const REG_CMD_MMIO_EN: u16 = 0b10;
pub const REG_CMD_BUSMSTR_EN: u16 = 0b100;
pub const REG_CMD_INTX_DIS: u16 = 0b100_00000000;
pub const REG_MASK_CMD: u16 =
    REG_CMD_IO_EN | REG_CMD_MMIO_EN | REG_CMD_BUSMSTR_EN | REG_CMD_INTX_DIS;

pub const BAR_TYPE_IO: u32 = 0b01;
pub const BAR_TYPE_MEM: u32 = 0b000;
pub const BAR_TYPE_MEM64: u32 = 0b100;
