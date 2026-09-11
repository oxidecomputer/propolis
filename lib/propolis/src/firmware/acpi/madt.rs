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

const IO_APIC_ID: u8 = 0x02;
const IO_APIC_GSI_BASE: u32 = 0x0000;

const TIMER_IRQ: u8 = 0;
const TIMER_GSI: u32 = 2;

const LOCAL_APIC_INT_NUMBER: u8 = 1;

/// Configuration for generating a MADT table.
pub struct MadtConfig {
    /// The ACPI table variant to use.
    pub acpi_variant: AcpiVariant,

    /// Number of vCPUs in the VM.
    pub num_cpus: u8,
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

        // Processor Local APIC.
        // ACPI rev. 6.6 section 5.2.12.2 "Processor Local APIC Structure"
        for i in 0..self.config.num_cpus {
            table.add_structure(madt::ProcessorLocalApic::new(
                i,
                i,
                madt::EnabledStatus::Enabled,
            ));
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

        // Local APIC NMI.
        // ACPI rev. 6.6 section 5.2.12.7 "Local APIC NMI Structure"
        //
        // Supporting more than 255 vCPUs will require additional work.
        // Refer to propolis#956 for more information.
        table.add_structure(madt::ProcessorLocalApicNmi::new(
            0xff, // Apply to all processors.
            LOCAL_APIC_INT_NUMBER,
        ));

        table.to_aml_bytes(sink);
    }
}
