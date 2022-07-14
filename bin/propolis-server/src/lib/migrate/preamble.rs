use propolis::instance::Instance;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug)]
pub(crate) enum MemType {
    RAM,
    ROM,
    Dev,
    Res,
}

// Memory regions in the guest physical address space.
#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct MemRegion {
    pub start: u64,
    pub end: u64,
    pub typ: MemType,
}

// PCI vendor and device ID pairs.
#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct PciId {
    pub vendor: u16,
    pub device: u16,
}

// PCI Bus/Device/Function.
#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct PciBdf {
    pub bus: u16,
    pub device: u8,
    pub function: u8,
}

// PIO ranges associated with some device.
#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct DevPorts {
    pub device: u32,
    pub ports: Vec<u16>,
}

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct VmDescr {
    pub vcpus: Vec<u32>,           // APIC IDs.
    pub ioapics: Vec<u32>,         // IOAPICs.
    pub mem: Vec<MemRegion>,       // Start, end, type
    pub pci: Vec<(PciId, PciBdf)>, // Vendor/ID + BDF
    pub ports: Vec<DevPorts>,
}

impl VmDescr {
    pub fn new(_instance: &Instance) -> VmDescr {
        // XXX: Just for demo purposes. We should get this from the instance.
        let vcpus = vec![0, 1, 2, 3];
        VmDescr {
            vcpus,
            ioapics: Vec::new(),
            mem: Vec::new(),
            pci: Vec::new(),
            ports: Vec::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub vm_descr: VmDescr,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance: &Instance) -> Preamble {
        Preamble { vm_descr: VmDescr::new(instance), blobs: Vec::new() }
    }
}
