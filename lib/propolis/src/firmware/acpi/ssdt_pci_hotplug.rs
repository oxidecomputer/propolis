// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use acpi_tables::{aml, sdt::Sdt, Aml, AmlSink};

use super::aml::{gpe, names, paths};
use super::GPE_PCI_HOTPLUG;

pub struct SsdtPciHotplug {}

impl SsdtPciHotplug {
    pub fn new() -> Self {
        Self {}
    }
}

impl Aml for SsdtPciHotplug {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut ssdt = Vec::new();

        aml::Scope::new(paths::gpe(), vec![&names::hid(&"ACPI0006")])
            .to_aml_bytes(&mut ssdt);

        aml::Scope::new(
            paths::gpe(),
            vec![&gpe::Method::new(
                GPE_PCI_HOTPLUG,
                gpe::Trigger::Edge,
                vec![&aml::Name::new("TEST".into(), &0_u8)],
            )],
        )
        .to_aml_bytes(&mut ssdt);

        let mut sdt = Sdt::new(*b"SSDT", 36, 1, *b"OXIDE ", *b"PCIHPLUG", 0x1);
        sdt.append_slice(ssdt.as_slice());
        sdt.to_aml_bytes(sink);
    }
}
