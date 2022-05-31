//! Definitions of various IDs assigned to the devices we expose to guest instances.

/// Oxide's IEE MA-L assignment.
pub const OXIDE_OUI: [u8; 3] = [0xa8, 0x40, 0x25];

/// PCI Specific IDs
pub mod pci {
    // Vendor IDs

    /// Oxide's PCI-SIG assigned Vendor ID.
    ///
    /// Devices emulating existing hardware will generally use the corresponding vendor &
    /// device IDs but specifiy a Oxide as the Subsystem Vendor. For virtual devices defined
    /// entirely by Propolis shall use Oxide's Vendor ID.
    pub const VENDOR_OXIDE: u16 = 0x1de;

    /// RedHat's PCI-SIG assigned Vendor ID, as used for Virtio devices.
    ///
    /// See Virtio 1.1 Section 4.1.2 PCI Device Discovery
    pub const VENDOR_VIRTIO: u16 = 0x1AF4;

    /// Intel's PCI-SIG assigned Vendor ID.
    pub const VENDOR_INTEL: u16 = 0x8086;

    // Emulated Device IDs

    /// PCI Device ID for the PIIX4 Host Bridge.
    pub const PIIX4_HB_DEV_ID: u16 = 0x1237;

    /// PCI Device ID for the PIIX3 ISA Controller.
    pub const PIIX3_ISA_DEV_ID: u16 = 0x7000;

    /// PCI Device ID for the PIIX4 ACPI PM Controller.
    pub const PIIX4_PM_DEV_ID: u16 = 0x7113;

    // Subsystem Device IDs (for devices emulated by propolis)

    /// PCI Subsystem Device ID for the PIIX4 Host Bridge as emulated by propolis.
    pub const PIIX4_HB_SUB_DEV_ID: u16 = 0xfffe;

    /// PCI Subsystem Device ID for the PIIX3 ISA Controller as emulated by propolis.
    pub const PIIX3_ISA_SUB_DEV_ID: u16 = 0xfffd;

    /// PCI Subsystem Device ID for the PIIX4 ACPI PM Controller as emulated by propolis.
    pub const PIIX4_PM_SUB_DEV_ID: u16 = 0xfffc;

    /// PCI Subsystem Device ID for the Propolis Virtio Network device.
    pub const VIRTIO_NET_SUB_DEV_ID: u16 = 0xfffb;

    /// PCI Subsystem Device ID for the Propolis Virtio Block device.
    pub const VIRTIO_BLOCK_SUB_DEV_ID: u16 = 0xfffa;

    // Propolis-specific Device IDs

    /// PCI Device ID for the Propolis NVMe controller.
    pub const PROPOLIS_NVME_DEV_ID: u16 = 0x0;

    /// PCI Device ID for the Propolis xHCI controller.
    pub const PROPOLIS_XHCI_DEV_ID: u16 = 0x1;

    /// PCI Device ID for the Propolis PCI-PCI bridge.
    pub const PROPOLIS_BRIDGE_DEV_ID: u16 = 0x2;
}
