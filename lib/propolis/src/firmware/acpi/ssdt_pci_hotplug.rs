// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::{PCI_HOTPLUG_STATUS_ADDR, PCI_HOTPLUG_STATUS_LEN};
use acpi_tables::{aml, sdt::Sdt, Aml, AmlSink};

const PCI_DSM_UUID: &str = "e5c937d0-3553-4d7a-9117-ea4d19c3434d";

const PCI_HOTPLUG_EJECT_ADDR: u16 = PCI_HOTPLUG_STATUS_ADDR + 0x08;
const PCI_HOTPLUG_EJECT_LEN: u8 = 4;

const PCI_HOTPLUG_BUS_NUMBER_ADDR: u16 = PCI_HOTPLUG_STATUS_ADDR + 0x10;
const PCI_HOTPLUG_BUS_NUMBER_LEN: u8 = 8;

const PCI_HOTPLUG_SIZE: u8 = 0x0018;
const PCI_HOTPLUG_DEVICES: u8 = 32;

pub struct SsdtPciHotplug {}

impl SsdtPciHotplug {
    pub fn new() -> Self {
        Self {}
    }
}

impl Aml for SsdtPciHotplug {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut table = Vec::new();

        aml::Scope::new(
            "\\_GPE".into(),
            vec![
                &aml::Name::new("_HID".into(), &"ACPI0006"),
                &aml::Method::new(
                    "_E01".into(),
                    0,
                    false,
                    vec![
                        &aml::Acquire::new("\\_SB_.PCI0.BLCK".into(), 0xffff),
                        &aml::MethodCall::new(
                            "\\_SB_.PCI0.PCNT".into(),
                            vec![],
                        ),
                        &aml::Release::new("\\_SB_.PCI0.BLCK".into()),
                    ],
                ),
            ],
        )
        .to_aml_bytes(&mut table);

        aml::Scope::new(
            "\\_SB_.PCI0".into(),
            vec![
                &aml::Device::new(
                    "PHPR".into(),
                    vec![
                        &aml::Name::new("_HID".into(), &"PNP0A06"),
                        &aml::Name::new("_UID".into(), &"PCI Hotplug resource"),
                        &aml::Name::new("_STA".into(), &0x0b_u8),
                        &aml::Name::new(
                            "_CRS".into(),
                            &aml::ResourceTemplate::new(vec![&aml::IO::new(
                                PCI_HOTPLUG_STATUS_ADDR,
                                PCI_HOTPLUG_STATUS_ADDR,
                                0x01,
                                PCI_HOTPLUG_SIZE,
                            )]),
                        ),
                    ],
                ),
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
                &aml::OpRegion::new(
                    "BNMR".into(),
                    aml::OpRegionSpace::SystemIO,
                    &PCI_HOTPLUG_BUS_NUMBER_ADDR,
                    &PCI_HOTPLUG_BUS_NUMBER_LEN,
                ),
                &aml::Field::new(
                    "BNMR".into(),
                    aml::FieldAccessType::DWord,
                    aml::FieldLockRule::NoLock,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![
                        aml::FieldEntry::Named(*b"BNUM", 32),
                        aml::FieldEntry::Named(*b"PIDX", 32),
                    ],
                ),
                &aml::Mutex::new("BLCK".into(), 0x00),
                &aml::Name::new("BSEL".into(), &aml::ZERO),
                &PciHotplugDevices::new(PCI_HOTPLUG_DEVICES),
            ],
        )
        .to_aml_bytes(&mut table);

        let mut sdt = Sdt::new(*b"SSDT", 36, 1, *b"OXIDE ", *b"PCIHPLUG", 0x1);
        sdt.append_slice(table.as_slice());
        sdt.to_aml_bytes(sink);
    }
}

struct PciHotplugDevices {
    devices: u8,
}

impl PciHotplugDevices {
    fn new(devices: u8) -> Self {
        Self { devices }
    }
}

impl Aml for PciHotplugDevices {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        let mut dvnt = Vec::new();

        for i in 3..self.devices {
            let name = format!("S{:02X}_", i * 8);
            PciHotplugDevice::new(name.clone(), i).to_aml_bytes(sink);
            dvnt.push(PciHotplugDeviceNotify::new(name.clone(), i));
        }

        aml::Method::new(
            "PCNT".into(),
            0,
            false,
            vec![
                &aml::Store::new(&aml::Path::new("BNUM"), &aml::ZERO),
                &aml::MethodCall::new(
                    "DVNT".into(),
                    vec![&aml::Path::new("PCIU"), &aml::ONE],
                ),
                &aml::MethodCall::new(
                    "DVNT".into(),
                    vec![&aml::Path::new("PCID"), &0x03_u8],
                ),
            ],
        )
        .to_aml_bytes(sink);

        aml::Method::new(
            "DVNT".into(),
            2,
            false,
            dvnt.iter().map(|e| e as &dyn Aml).collect(),
        )
        .to_aml_bytes(sink);

        aml::Method::new(
            "PCEJ".into(),
            2,
            false,
            vec![
                &aml::Acquire::new("BLCK".into(), 0xffff),
                &aml::Store::new(&aml::Path::new("BNUM"), &aml::Arg(0)),
                &aml::ShiftLeft::new(
                    &aml::Path::new("B0EJ"),
                    &aml::ONE,
                    &aml::Arg(1),
                ),
                &aml::Release::new("BLCK".into()),
                &aml::Return::new(&aml::ZERO),
            ],
        )
        .to_aml_bytes(sink);

        aml::Method::new(
            "PDSM".into(),
            5,
            true,
            vec![
                &aml::If::new(
                    &aml::Equal::new(&aml::Arg(2), &aml::ZERO),
                    vec![
                        &aml::Store::new(
                            &aml::Local(0),
                            &aml::BufferData::new(vec![0x00]),
                        ),
                        &aml::If::new(
                            &aml::NotEqual::new(
                                &aml::Arg(0),
                                &aml::Uuid::new(PCI_DSM_UUID),
                            ),
                            vec![&aml::Return::new(&aml::Local(0))],
                        ),
                        &aml::If::new(
                            &aml::LessThan::new(&aml::Arg(1), &0x02_u8),
                            vec![&aml::Return::new(&aml::Local(0))],
                        ),
                        &aml::Store::new(&aml::Local(1), &aml::ZERO),
                        &aml::Store::new(
                            &aml::Local(2),
                            &aml::MethodCall::new(
                                "AIDX".into(),
                                vec![
                                    &aml::DeRefOf::new(&aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Arg(4),
                                        &aml::ZERO,
                                    )),
                                    &aml::DeRefOf::new(&aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Arg(4),
                                        &aml::ONE,
                                    )),
                                ],
                            ),
                        ),
                        &aml::If::new(
                            &aml::LogicalNot::new(&aml::Or::new(
                                &aml::ZERO,
                                &aml::Equal::new(&aml::Local(2), &aml::ZERO),
                                &aml::Equal::new(
                                    &aml::Local(2),
                                    &0xffffffff_u32,
                                ),
                            )),
                            vec![
                                &aml::Or::new(
                                    &aml::Local(1),
                                    &aml::Local(1),
                                    &aml::ONE,
                                ),
                                &aml::Or::new(
                                    &aml::Local(1),
                                    &aml::Local(1),
                                    &aml::ShiftLeft::new(
                                        &aml::ZERO,
                                        &aml::ONE,
                                        &0x07_u8,
                                    ),
                                ),
                            ],
                        ),
                        &aml::Store::new(
                            &aml::Index::new(
                                &aml::ZERO,
                                &aml::Local(0),
                                &aml::ZERO,
                            ),
                            &aml::Local(1),
                        ),
                        &aml::Return::new(&aml::Local(0)),
                    ],
                ),
                &aml::If::new(
                    &aml::Equal::new(&aml::Arg(2), &0x07_u8),
                    vec![
                        &aml::Store::new(
                            &aml::Local(2),
                            &aml::MethodCall::new(
                                "AIDX".into(),
                                vec![
                                    &aml::DeRefOf::new(&aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Arg(4),
                                        &aml::ZERO,
                                    )),
                                    &aml::DeRefOf::new(&aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Arg(4),
                                        &aml::ONE,
                                    )),
                                ],
                            ),
                        ),
                        &aml::Store::new(
                            &aml::Local(0),
                            // TODO: make empty package
                            &aml::Package::new(vec![&aml::ZERO, &aml::ZERO]),
                        ),
                        &aml::If::new(
                            &aml::LogicalNot::new(&aml::LogicalOr::new(
                                &aml::Equal::new(&aml::Local(2), &aml::ZERO),
                                &aml::Equal::new(
                                    &aml::Local(2),
                                    &0xffffffff_u32,
                                ),
                            )),
                            vec![
                                &aml::Store::new(
                                    &aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Local(0),
                                        &aml::ZERO,
                                    ),
                                    &aml::Local(2),
                                ),
                                &aml::Store::new(
                                    &aml::Index::new(
                                        &aml::ZERO,
                                        &aml::Local(0),
                                        &aml::ONE,
                                    ),
                                    &"",
                                ),
                            ],
                        ),
                        &aml::Return::new(&aml::Local(0)),
                    ],
                ),
            ],
        )
        .to_aml_bytes(sink);

        aml::Method::new(
            "AIDX".into(),
            2,
            false,
            vec![
                &aml::Acquire::new("BLCK".into(), 0xffff),
                &aml::Store::new(&aml::Path::new("BNUM"), &aml::Arg(0)),
                &aml::ShiftLeft::new(
                    &aml::Path::new("PIDX"),
                    &aml::ONE,
                    &aml::Arg(1),
                ),
                &aml::Store::new(&aml::Local(0), &aml::Path::new("PIDX")),
                &aml::Release::new("BLCK".into()),
                &aml::Return::new(&aml::Local(0)),
            ],
        )
        .to_aml_bytes(sink);
    }
}

struct PciHotplugDevice {
    name: String,
    number: u8,
}

impl PciHotplugDevice {
    fn new(name: String, number: u8) -> Self {
        Self { name, number }
    }
}

impl Aml for PciHotplugDevice {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        aml::Device::new(
            aml::Path::new(&self.name),
            vec![
                &aml::Name::new(
                    "_ADR".into(),
                    &(self.number as u32 * 0x10000_u32),
                ),
                &aml::Name::new("ASUN".into(), &self.number),
                &aml::Name::new("_SUN".into(), &self.number),
                &aml::Method::new(
                    "_DSM".into(),
                    4,
                    true,
                    vec![
                        &aml::Store::new(
                            &aml::Local(0),
                            &aml::Package::new(vec![&aml::ZERO, &aml::ZERO]),
                        ),
                        &aml::Store::new(
                            &aml::Index::new(
                                &aml::ZERO,
                                &aml::Local(0),
                                &aml::ZERO,
                            ),
                            &aml::Path::new("BSEL"),
                        ),
                        &aml::Store::new(
                            &aml::Index::new(
                                &aml::ZERO,
                                &aml::Local(0),
                                &aml::ONE,
                            ),
                            &aml::Path::new("ASUN"),
                        ),
                        &aml::Return::new(&aml::MethodCall::new(
                            "PDSM".into(),
                            vec![
                                &aml::Arg(0),
                                &aml::Arg(1),
                                &aml::Arg(2),
                                &aml::Arg(3),
                                &aml::Local(0),
                            ],
                        )),
                    ],
                ),
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

struct PciHotplugDeviceNotify {
    name: String,
    number: u8,
}

impl PciHotplugDeviceNotify {
    fn new(name: String, number: u8) -> Self {
        Self { name, number }
    }
}

impl Aml for PciHotplugDeviceNotify {
    fn to_aml_bytes(&self, sink: &mut dyn AmlSink) {
        aml::If::new(
            &aml::And::new(&aml::ZERO, &aml::Arg(0), &(1_u32 << self.number)),
            vec![&aml::Notify::new(&aml::Path::new(&self.name), &aml::Arg(1))],
        )
        .to_aml_bytes(sink);
    }
}
