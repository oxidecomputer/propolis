pub const VIRTIO_DEV_NET: u16 = 0x1000;
pub const VIRTIO_DEV_BLOCK: u16 = 0x1001;

// Legacy interface feature bits
pub const VIRTIO_F_NOTIFY_ON_EMPTY: usize = 1 << 24;
pub const VIRTIO_F_ANY_LAYOUT: usize = 1 << 27;

// Standard interface feature bits
pub const VIRTIO_F_RING_INDIRECT_DESC: usize = 1 << 28;
pub const VIRTIO_F_RING_EVENT_IDX: usize = 1 << 29;
pub const VIRTIO_F_VERSION_1: usize = 1 << 32;

// virtio-net feature bits
pub const VIRTIO_NET_F_CSUM: u32 = 1 << 0;
pub const VIRTIO_NET_F_GUEST_CSUM: u32 = 1 << 1;
pub const VIRTIO_NET_F_CTRL_GUEST_OFFLOADS: u32 = 1 << 2;
pub const VIRTIO_NET_F_MTU: u32 = 1 << 3;
pub const VIRTIO_NET_F_MAC: u32 = 1 << 5;
pub const VIRTIO_NET_F_GUEST_TSO4: u32 = 1 << 7;
pub const VIRTIO_NET_F_GUEST_TSO6: u32 = 1 << 8;
pub const VIRTIO_NET_F_GUEST_ECN: u32 = 1 << 9;
pub const VIRTIO_NET_F_GUEST_UFO: u32 = 1 << 10;
pub const VIRTIO_NET_F_HOST_TSO4: u32 = 1 << 11;
pub const VIRTIO_NET_F_HOST_TSO6: u32 = 1 << 12;
pub const VIRTIO_NET_F_HOST_ECN: u32 = 1 << 13;
pub const VIRTIO_NET_F_HOST_UFO: u32 = 1 << 14;
pub const VIRTIO_NET_F_MGR_RXBUF: u32 = 1 << 15;
pub const VIRTIO_NET_F_STATUS: u32 = 1 << 16;
pub const VIRTIO_NET_F_CTRL_VQ: u32 = 1 << 17;
pub const VIRTIO_NET_F_CTRL_RX: u32 = 1 << 18;
pub const VIRTIO_NET_F_CTRL_VLAN: u32 = 1 << 19;

// virtio-block feature bits
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1 << 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 1 << 2;
pub const VIRTIO_BLK_F_GEOMETRY: u32 = 1 << 4;
pub const VIRTIO_BLK_F_RO: u32 = 1 << 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 1 << 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 1 << 9;
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 1 << 10;
pub const VIRTIO_BLK_F_CONFIG_WCE: u32 = 1 << 11;
pub const VIRTIO_BLK_F_DISCARD: u32 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 1 << 14;
