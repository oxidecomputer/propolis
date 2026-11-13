// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::aml::{devids, io_port, names, *};
use acpi_tables::{aml, sdt::Sdt, Aml, AmlSink};

pub const PCI_HOTPLUG_BASE_ADDR: u16 = 0xae00;
pub const PCI_HOTPLUG_BASE_LEN: u8 = 0x18;

const PCI_HOTPLUG_STATUS_ADDR: u16 = PCI_HOTPLUG_BASE_ADDR;
const PCI_HOTPLUG_STATUS_LEN: u8 = 0x08;

const PCI_HOTPLUG_EJECT_ADDR: u16 = PCI_HOTPLUG_BASE_ADDR + 0x08;
const PCI_HOTPLUG_EJECT_LEN: u8 = 0x04;

const PCI_HOTPLUG_BUS_ADDR: u16 = PCI_HOTPLUG_BASE_ADDR + 0x10;
const PCI_HOTPLUG_BUS_LEN: u8 = 0x04;

pub struct SsdtPciHotplug {
    pub slots: Vec<u8>,
}
impl Aml for SsdtPciHotplug {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let devices: Vec<_> =
            self.slots.iter().map(|s| SlotDevice::new(*s)).collect();

        let mut ssdt = Vec::new();

        // Scope (_GPE)
        aml::Scope::new(
            "\\_GPE".into(),
            vec![
                // Method (_E01)
                &aml::Method::new(
                    "_E01".into(),
                    0,
                    false,
                    vec![
                        &aml::Acquire::new("\\_SB_.PCI0.BLCK".into(), 0xFFFF),
                        &aml::MethodCall::new(
                            "\\_SB_.PCI0.PCNT".into(),
                            vec![],
                        ),
                        &aml::Release::new("\\_SB_.PCI0.BLCK".into()),
                    ],
                ),
            ],
        )
        .to_aml_bytes(&mut ssdt);

        // Scope (\_SB.PCI0)
        aml::Scope::new(
            "\\_SB_.PCI0".into(),
            vec![
                // Device (PHPR)
                &aml::Device::new(
                    "PHPR".into(),
                    vec![
                        &names::hid(&devids::GENERIC_CONTAINER),
                        &names::uid(&"PCI Hotplug resources"),
                        &names::sta(
                            DeviceStatus::PRESENT
                                | DeviceStatus::ENABLED
                                | DeviceStatus::FUNCTIONING,
                        ),
                        &names::crs(&aml::ResourceTemplate::new(vec![
                            &io_port(
                                PCI_HOTPLUG_BASE_ADDR,
                                0x01,
                                PCI_HOTPLUG_BASE_LEN,
                            ),
                        ])),
                    ],
                ),
                // OperationRegion (PCST)
                &aml::OpRegion::new(
                    "PCST".into(),
                    aml::OpRegionSpace::SystemIO,
                    &PCI_HOTPLUG_STATUS_ADDR,
                    &PCI_HOTPLUG_STATUS_LEN,
                ),
                &aml::Field::new(
                    "PCST".into(),
                    aml::FieldAccessType::DWord,
                    aml::FieldLockRule::NoLock,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![
                        aml::FieldEntry::Named(*b"PCIU", 32),
                        aml::FieldEntry::Named(*b"PCID", 32),
                    ],
                ),
                // OperationRegion (SEJ)
                &aml::OpRegion::new(
                    "SEJ_".into(),
                    aml::OpRegionSpace::SystemIO,
                    &PCI_HOTPLUG_EJECT_ADDR,
                    &PCI_HOTPLUG_EJECT_LEN,
                ),
                &aml::Field::new(
                    "SEJ_".into(),
                    aml::FieldAccessType::DWord,
                    aml::FieldLockRule::NoLock,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![aml::FieldEntry::Named(*b"B0EJ", 32)],
                ),
                // OperationRegion (BNMR)
                &aml::OpRegion::new(
                    "BNMR".into(),
                    aml::OpRegionSpace::SystemIO,
                    &PCI_HOTPLUG_BUS_ADDR,
                    &PCI_HOTPLUG_BUS_LEN,
                ),
                &aml::Field::new(
                    "BNMR".into(),
                    aml::FieldAccessType::DWord,
                    aml::FieldLockRule::NoLock,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![aml::FieldEntry::Named(*b"BNUM", 32)],
                ),
                // Mutex (BLCK)
                &aml::Mutex::new("BLCK".into(), 0x0),
                // Name (BSEL)
                &aml::Name::new("BSEL".into(), &aml::ZERO),
                // Method (PCNT)
                &aml::Method::new(
                    "PCNT".into(),
                    0,
                    NOT_SERIALIZED,
                    vec![
                        &aml::Store::new(&aml::Path::new("BNUM"), &aml::ZERO),
                        &aml::MethodCall::new(
                            "DVNT".into(),
                            vec![&aml::Path::new("PCIU"), &aml::ONE],
                        ),
                        &aml::MethodCall::new(
                            "DVNT".into(),
                            vec![&aml::Path::new("PCID"), &0x03_u64],
                        ),
                    ],
                ),
                // Method (DVNT)
                &aml::Method::new(
                    "DVNT".into(),
                    2,
                    NOT_SERIALIZED,
                    vec![&DvntBody { slots: &devices }],
                ),
                // Method (PCEJ)
                &aml::Method::new(
                    "PCEJ".into(),
                    2,
                    NOT_SERIALIZED,
                    vec![
                        &aml::Acquire::new("BLCK".into(), 0xffff),
                        &aml::Store::new(&aml::Path::new("BNUM"), &aml::Arg(0)),
                        &aml::Store::new(
                            &aml::Path::new("B0EJ"),
                            &aml::ShiftLeft::new(
                                &aml::ZERO,
                                &aml::ONE,
                                &aml::Arg(1),
                            ),
                        ),
                        &aml::Release::new("BLCK".into()),
                        &aml::Return::new(&aml::ZERO),
                    ],
                ),
            ],
        )
        .to_aml_bytes(&mut ssdt);

        aml::Scope::new(
            "\\_SB_.PCI0".into(),
            // Device (Dxxx)
            devices.iter().map(|d| d as &dyn Aml).collect(),
        )
        .to_aml_bytes(&mut ssdt);

        // SSDT table.
        let mut sdt = Sdt::new(*b"SSDT", 36, 1, *b"OXIDE ", *b"PCIHPLUG", 0x1);
        sdt.append_slice(ssdt.as_slice());
        sdt.to_aml_bytes(sink);
    }
}

struct DvntBody<'a> {
    pub slots: &'a [SlotDevice],
}
impl<'a> Aml for DvntBody<'a> {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        for slot in self.slots {
            aml::If::new(
                &aml::LogicalAnd::new(&aml::Arg(0), &(1_u64 << slot.number)),
                vec![&aml::Notify::new(
                    &aml::Path::new(&slot.name),
                    &aml::Arg(1),
                )],
            )
            .to_aml_bytes(sink)
        }
    }
}

struct SlotDevice {
    name: String,
    number: u8,
    addr: u64,
}
impl SlotDevice {
    fn new(number: u8) -> Self {
        SlotDevice {
            name: format!("D{:02}0", number),
            number,
            addr: (number as u64) << 16,
        }
    }
}
impl Aml for SlotDevice {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        aml::Device::new(
            self.name.as_str().into(),
            vec![
                &names::adr(&self.addr),
                &names::sun(&self.number),
                &aml::Method::new(
                    "_EJ0".into(),
                    1,
                    false,
                    vec![&aml::MethodCall::new(
                        "PCEJ".into(),
                        vec![&aml::Path::new("BSEL"), &aml::Path::new("_SUN")],
                    )],
                ),
            ],
        )
        .to_aml_bytes(sink);
    }
}
