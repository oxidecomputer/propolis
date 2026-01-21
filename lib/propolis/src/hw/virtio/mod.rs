// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Propolis implements VirtIO devices for guests with appropriate drivers.
//!
//! We model virtio devices as (virtual) PCI devices, using the virtio PCI
//! transport mechanism as defined in the VirtIO 1.2 specification.
//! Currently we expose drivers for virtio-net, virtio-block, and virtio-9pfs.

use bitflags::bitflags;

#[allow(unused)]
mod bits;

pub mod block;
#[cfg(feature = "falcon")]
pub mod p9fs;
pub mod pci;
mod queue;
#[cfg(feature = "falcon")]
pub mod softnpu;
pub mod viona;

use crate::common::RWOp;
use crate::hw::pci as pci_hw;
use crate::lifecycle::Lifecycle;
use queue::VirtQueue;

pub use block::PciVirtioBlock;
pub use viona::PciVirtioViona;

bitflags! {
    pub struct LegacyFeatures: u64 {
        const NOTIFY_ON_EMPTY = 1 << 24;
        const ANY_LAYOUT = 1 << 27;
    }
}

/// Describes the VirtIO "mode" exposed by the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::FromRepr)]
#[repr(u32)]
pub enum Mode {
    /// Legacy mode is pre-VirtIO 1.0.
    Legacy,

    /// Modern devices are those that implement and expose the VirtIO
    /// 1.0 and later specification.
    Modern,

    /// Transitional devices exposes both the pre-VirtIO 1.0 "Legacy"
    /// interface VirtIO 1.0 and later "Modern" interface.
    Transitional,
}

impl Mode {
    /// Returns the PCI revision ID for the given mode.
    pub fn pci_revision(self) -> u8 {
        match self {
            Mode::Legacy | Mode::Transitional => 0,
            Mode::Modern => 1,
        }
    }
}

/// Recognized VirtIO Device IDs, as defined in the VirtIO 1.2 specification,
/// section 5, "Device Types".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceId {
    Reserved = 0,
    Network = 1,
    Block = 2,
    Console = 3,
    Entropy = 4,
    TradMemBalloon = 5,
    IoMem = 6,
    RpMsg = 7,
    Scsi = 8,
    NineP = 9,
    Mac80211Wlan = 10,
    RprocSerial = 11,
    Caif = 12,
    MemBalloon = 13,
    Gpu = 16,
    Timer = 17,
    Input = 18,
    Socket = 19,
    Crypto = 20,
    SigDistMod = 21,
    Pstore = 22,
    Iommu = 23,
    Memory = 24,
    Audio = 25,
    Filesystem = 26,
    Pmem = 27,
    Rpmb = 28,
    Mac80211HWSim = 29,
    VideoEncoder = 30,
    VideoDecoder = 31,
    ArmScmi = 32,
    NitroSecureMod = 33,
    I2c = 34,
    Watchdog = 35,
    Can = 36,
    ParameterServer = 38,
    AudioPolicy = 39,
    Bluetooth = 40,
    Gpio = 41,
    Rdma = 42,
}

impl DeviceId {
    /// Maps a VirtIO Device ID to a PCI Device ID for the given mode.
    ///
    /// VirtIO defines its own namespace for device IDs that is independent
    /// of the underlying transport between host and guest.  The mapping from
    /// that space into PCI device IDs is dependent on the mode; for devices
    /// following the VirtIO 1.0 and later specifications, this is straight
    /// forward: just add 0x1040 to the VirtIO ID.
    ///
    /// However, for legacy and transitional mode devices, the mapping is
    /// irregular, and a table in the VirtIO specification lists the defined
    /// subset of device types and their respective PCI IDs. But note that there
    /// are legacy devices with no such defined mapping, and thus no standard
    /// transitional IDs. In these cases, we choose to use IDs that seem to be
    /// shared in a broad consensus across different implementations, in
    /// particular, QEMU.
    ///
    /// This is not really an issue for us, since we only expose a handful of
    /// device models; regardless, we provide mappings for everything defined in
    /// the VirtIO spec.
    ///
    /// See VirtIO 1.2, sec 4.1.2.1 for the mapping from VirtIO device ID
    /// to PCI device ID.
    pub fn pci_dev_id(self, mode: Mode) -> Result<u16, Self> {
        match mode {
            Mode::Modern => Ok(self as u16 + 0x1040),
            Mode::Legacy | Mode::Transitional => match self {
                Self::Network => Ok(0x1000),
                Self::Block => Ok(0x1001),
                Self::TradMemBalloon => Ok(0x1002),
                Self::Console => Ok(0x1003),
                Self::Scsi => Ok(0x1004),
                Self::Entropy => Ok(0x1005),
                Self::NineP => Ok(0x1009),
                Self::Socket => Ok(0x1012), // Taken from QEMU, used by Linux
                _ => Err(self),
            },
        }
    }

    /// Maps a VirtIO Device ID to a PCI Device Sub ID.
    /// XXX: Check these mappings against some reference.
    pub fn pci_sub_dev_id(self, mode: Mode) -> Result<u16, Self> {
        match mode {
            Mode::Legacy | Mode::Transitional => Ok(self as u16),
            Mode::Modern => self.pci_dev_id(mode),
        }
    }

    /// Maps a VirtIO Device ID to a PCI Device Class.
    ///
    /// Sadly, these mappings are mostly arbitrary.
    pub fn pci_class(self) -> Result<u8, Self> {
        match self {
            Self::Network => Ok(pci_hw::bits::CLASS_NETWORK),
            Self::Block | Self::NineP => Ok(pci_hw::bits::CLASS_STORAGE),
            _ => Err(self),
        }
    }

    /// Constructs a crate::hw::pci::Ident from the given VirtIO device
    /// ID and mode.
    pub fn pci_ident(self, mode: Mode) -> Result<pci_hw::Ident, Self> {
        use crate::hw::ids::pci::VENDOR_VIRTIO;
        let vendor_id = VENDOR_VIRTIO;
        let sub_vendor_id = VENDOR_VIRTIO;
        let device_id = self.pci_dev_id(mode)?;
        let sub_device_id = self.pci_sub_dev_id(mode)?;
        let device_class = self.pci_class()?;
        let revision_id = mode.pci_revision();
        Ok(pci_hw::Ident {
            vendor_id,
            device_id,
            sub_vendor_id,
            sub_device_id,
            device_class,
            revision_id,
            ..Default::default()
        })
    }
}

pub trait VirtioDevice: Send + Sync + 'static + Lifecycle {
    /// Read/write device-specific virtio configuration space.
    fn rw_dev_config(&self, ro: RWOp);

    /// Returns the device virtio mode (Legacy, Transitional, Modern).
    fn mode(&self) -> Mode;

    /// Returns the device-specific virtio feature bits.
    fn features(&self) -> u64;

    /// Sets the device-specific virtio feature bits
    ///
    /// Returns `Err` if an error occurred while setting the features.  Doing so
    /// will transition the device to the Failed state.
    fn set_features(&self, feat: u64) -> Result<(), ()>;

    /// Service driver notification for a given virtqueue
    fn queue_notify(&self, vq: &VirtQueue);

    /// Notification of virtqueue configuration change
    ///
    /// Returns `Err` if an error occurred while handling the specified
    /// `VqChange`.  Doing so will transition the device to the Failed state.
    fn queue_change(
        &self,
        _vq: &VirtQueue,
        _change: VqChange,
    ) -> Result<(), ()> {
        Ok(())
    }
}

pub trait VirtioIntr: Send + 'static {
    fn notify(&self);
    fn read(&self) -> VqIntr;
}

pub enum VqChange {
    /// Underlying virtio device has been reset
    Reset,

    /// Physical address changed for VQ
    Address,

    /// MSI(-X) configuration changed for VQ
    IntrCfg,
}

pub enum VqIntr {
    /// Pin (lintr) interrupt
    Pin,

    /// MSI(-X) with address, data, and masked state
    Msi(u64, u32, bool),
}

#[usdt::provider(provider = "propolis")]
mod probes {
    fn virtio_vq_notify(virtio_dev_addr: u64, virtqueue_id: u16) {}
    fn virtio_vq_pop(vq_addr: u64, desc_idx: u16, avail_idx: u16) {}
    fn virtio_vq_push(vq_addr: u64, used_idx: u16, used_len: u32) {}
}
