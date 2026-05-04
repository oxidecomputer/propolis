// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generates a MADT/APIC ACPI table for an instance.
//!
//! The [`Madt`] struct implements the `Aml` trait of the `acpi_tables` crate
//! and can write the AML bytecode to any AmlSink, like a `Vec<u8>`.

use super::{
    IO_APIC_ADDR, LOCAL_APIC_ADDR, OEM_ID, OEM_REVISION, OEM_TABLE_ID,
    PCI_LINK_IRQS,
};
use acpi_tables::{madt, Aml, AmlSink};

const IO_APIC_ID: u8 = 0x02;
const IO_APIC_GSI_BASE: u32 = 0x0000;

const TIMER_IRQ: u8 = 0;
const TIMER_GSI: u32 = 2;

const LOCAL_APIC_INT_NUMBER: u8 = 1;

pub struct MadtConfig {
    pub num_cpus: u8,
}

/// The MADT/APIC table describes the interrupts for the entire system.
///
/// <https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#multiple-apic-description-table-madt>
pub struct Madt<'a> {
    config: &'a MadtConfig,
}

impl<'a> Madt<'a> {
    pub fn new(config: &'a MadtConfig) -> Self {
        Self { config }
    }
}

// XXX(acpi): Values retained from the original EDK2 static tables.
//            fwts reports 3 medium failures for this table:
//              - madt: LAPIC has no matching processor UID 0
//              - madt: LAPIC has no matching processor UID 1
//              - madt: LAPICNMI has no matching processor UID 255
//
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiTables/Madt.aslc
// https://github.com/oxidecomputer/edk2/blob/f33871f488bfbbc080e0f7e3881e04d0db0b6367/OvmfPkg/AcpiPlatformDxe/Qemu.c#L58
impl<'a> Aml for Madt<'a> {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut table = madt::MADT::new(
            *OEM_ID,
            *OEM_TABLE_ID,
            OEM_REVISION,
            madt::LocalInterruptController::Address(LOCAL_APIC_ADDR),
        )
        .pc_at_compat();

        // Processor Local APIC.
        // https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#processor-local-apic-structure
        for i in 0..self.config.num_cpus {
            table.add_structure(madt::ProcessorLocalApic::new(
                i,
                i,
                madt::EnabledStatus::Enabled,
            ));
        }

        // I/O APIC.
        // https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#i-o-apic-structure
        table.add_structure(madt::IoApic::new(
            IO_APIC_ID,
            IO_APIC_ADDR,
            IO_APIC_GSI_BASE,
        ));

        // Interrupt Source Overrides.
        // https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#interrupt-source-override-structure
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
        // https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html#local-apic-nmi-structure
        table.add_structure(madt::ProcessorLocalApicNmi::new(
            0xff, // Apply to all processors.
            LOCAL_APIC_INT_NUMBER,
        ));

        table.to_aml_bytes(sink);
    }
}
