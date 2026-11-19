// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a MADT/APIC ACPI table for an instance.
//!
//! The [`Madt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{
    AcpiVariant, IO_APIC_ADDR, LOCAL_APIC_ADDR, OEM_ID, OEM_REVISION,
    OEM_TABLE_ID, PCI_LINK_IRQS,
};
use acpi_tables::{madt, Aml, AmlSink};
use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::{Immutable, IntoBytes};

const IO_APIC_ID: u8 = 0x02;
const IO_APIC_GSI_BASE: u32 = 0x0000;

const TIMER_IRQ: u8 = 0;
const TIMER_GSI: u32 = 2;

const LOCAL_APIC_INT_NUMBER: u8 = 1;

/// UID that refers to all processors in a Local xAPIC NMI structure.
const XAPIC_ALL_PROCESSORS_UID: u8 = 0xff;

/// UID that refers to all processors in a Local x2APIC NMI structure.
const X2APIC_ALL_PROCESSORS_UID: u32 = 0xffff_ffff;

/// Processors with APIC IDs at or above this value must be described with
/// the x2APIC variants of the Processor Local APIC structures.
///
/// ACPI rev. 6.6 section 5.2.12.12 "Processor Local x2APIC Structure"
const X2APIC_ID_THRESHOLD: u32 = 0xff;

/// Configuration for generating a MADT table.
pub struct MadtConfig {
    /// The ACPI table variant to use.
    pub acpi_variant: AcpiVariant,

    /// (x2)APIC IDs of the VM's vCPUs, in ascending order.
    pub apic_ids: Vec<u32>,
}

/// The MADT/APIC table describes the interrupts for the entire system.
///
/// ACPI rev. 6.6 section 5.2.12 "Multiple APIC Description Table (MADT)"
pub struct Madt {
    config: MadtConfig,
}

impl Madt {
    pub fn new(config: MadtConfig) -> Self {
        Self { config }
    }
}

// Values retained from the original EDK2 static tables.
//
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiTables/Madt.aslc
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiPlatformDxe/Qemu.c#L58
//
// fwts currently reports 3 medium failures for this table:
//   - madt: LAPIC has no matching processor UID 0
//   - madt: LAPIC has no matching processor UID 1
//   - madt: LAPICNMI has no matching processor UID 255
impl Aml for Madt {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut table = madt::MADT::new(
            *OEM_ID,
            *OEM_TABLE_ID,
            OEM_REVISION,
            madt::LocalInterruptController::Address(LOCAL_APIC_ADDR),
        )
        .pc_at_compat();

        // Processor Local APIC and Processor Local x2APIC.
        // ACPI rev. 6.6 section 5.2.12.2 "Processor Local APIC Structure"
        // ACPI rev. 6.6 section 5.2.12.12 "Processor Local x2APIC Structure"
        //
        // Each processor's UID is its (x2)APIC ID.
        let mut has_x2apic = false;
        for &apic_id in self.config.apic_ids.iter() {
            if apic_id < X2APIC_ID_THRESHOLD {
                let apic_id = apic_id as u8;
                table.add_structure(madt::ProcessorLocalApic::new(
                    apic_id,
                    apic_id,
                    madt::EnabledStatus::Enabled,
                ));
            } else {
                has_x2apic = true;
                table.add_structure(ProcessorLocalX2Apic::new(
                    apic_id,
                    apic_id,
                    madt::EnabledStatus::Enabled,
                ));
            }
        }

        // I/O APIC.
        // ACPI rev. 6.6 section 5.2.12.3 "I/O APIC Structure"
        table.add_structure(madt::IoApic::new(
            IO_APIC_ID,
            IO_APIC_ADDR,
            IO_APIC_GSI_BASE,
        ));

        // Interrupt Source Overrides.
        // ACPI rev. 6.6 section 5.2.12.5 "Interrupt Source Override Structure"
        table.add_structure(madt::InterruptSourceOverride::new(
            TIMER_IRQ, TIMER_GSI,
        ));

        // Set level-triggered and active high for all PCI link targets.
        PCI_LINK_IRQS.iter().for_each(|&i| {
            table.add_structure(
                madt::InterruptSourceOverride::new(i, i as u32)
                    .level_triggered()
                    .active_high(),
            );
        });

        // Local APIC NMI or Local x2APIC NMI.
        // ACPI rev. 6.6 section 5.2.12.7 "Local APIC NMI Structure"
        // ACPI rev. 6.6 section 5.2.12.13 "Local x2APIC NMI Structure"
        //
        // Both apply to all processors via their "all processors" UIDs. If
        // any processor has an x2APIC ID of 255 or greater, the global LINTx
        // for every processor must be described with a Local x2APIC NMI
        // structure. Otherwise, a Local APIC NMI structure is used.
        //
        // Supporting more than 255 vCPUs will require additional work.
        // Refer to propolis#956 for more information.
        if has_x2apic {
            table.add_structure(LocalX2ApicNmi::new(
                X2APIC_ALL_PROCESSORS_UID,
                LOCAL_APIC_INT_NUMBER,
            ));
        } else {
            table.add_structure(madt::ProcessorLocalApicNmi::new(
                XAPIC_ALL_PROCESSORS_UID,
                LOCAL_APIC_INT_NUMBER,
            ));
        }

        table.to_aml_bytes(sink);
    }
}

// XXX `acpi_tables` only provides the xAPIC flavors of these structures

/// Processor Local x2APIC structure.
///
/// ACPI rev. 6.6 section 5.2.12.12 "Processor Local x2APIC Structure"
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable)]
struct ProcessorLocalX2Apic {
    r#type: u8,
    length: u8,
    _reserved: U16,
    x2apic_id: U32,
    flags: U32,
    acpi_processor_uid: U32,
}

impl ProcessorLocalX2Apic {
    const TYPE: u8 = 0x9;

    fn new(uid: u32, x2apic_id: u32, enabled: madt::EnabledStatus) -> Self {
        Self {
            r#type: Self::TYPE,
            length: std::mem::size_of::<Self>() as u8,
            _reserved: 0.into(),
            x2apic_id: x2apic_id.into(),
            flags: (enabled as u32).into(),
            acpi_processor_uid: uid.into(),
        }
    }
}

impl Aml for ProcessorLocalX2Apic {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        sink.vec(self.as_bytes());
    }
}

/// Local x2APIC NMI structure.
///
/// ACPI rev. 6.6 section 5.2.12.13 "Local x2APIC NMI Structure"
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable)]
struct LocalX2ApicNmi {
    r#type: u8,
    length: u8,
    flags: U16,
    acpi_processor_uid: U32,
    local_x2apic_lint_num: u8,
    _reserved: [u8; 3],
}

impl LocalX2ApicNmi {
    const TYPE: u8 = 0xa;

    fn new(acpi_processor_uid: u32, local_x2apic_lint_num: u8) -> Self {
        Self {
            r#type: Self::TYPE,
            length: std::mem::size_of::<Self>() as u8,
            flags: 0.into(),
            acpi_processor_uid: acpi_processor_uid.into(),
            local_x2apic_lint_num,
            _reserved: [0; 3],
        }
    }
}

impl Aml for LocalX2ApicNmi {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        sink.vec(self.as_bytes());
    }
}
