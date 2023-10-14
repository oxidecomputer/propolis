// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bits copied from propolis-server, rather than splitting them out into some
//! shared dependency

use propolis_client::handmade::api;
use propolis_client::instance_spec::{v0::builder::SpecBuilderError, PciPath};

use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub(crate) enum SlotType {
    Nic,
    Disk,
    CloudInit,
}

#[allow(unused)]
#[derive(Debug, Error)]
pub(crate) enum ServerSpecBuilderError {
    #[error(transparent)]
    InnerBuilderError(#[from] SpecBuilderError),

    #[error("The string {0} could not be converted to a PCI path")]
    PciPathNotParseable(String),

    #[error(
        "Could not translate PCI slot {0} for device type {1:?} to a PCI path"
    )]
    PciSlotInvalid(u8, SlotType),

    #[error("Unrecognized storage device interface {0}")]
    UnrecognizedStorageDevice(String),

    #[error("Unrecognized storage backend type {0}")]
    UnrecognizedStorageBackend(String),

    #[error("Device {0} requested missing backend {1}")]
    DeviceMissingBackend(String, String),

    #[error("Error in server config TOML: {0}")]
    ConfigTomlError(String),

    #[error("Error serializing {0} into spec element: {1}")]
    SerializationError(String, serde_json::error::Error),
}

pub(crate) fn slot_to_pci_path(
    slot: api::Slot,
    ty: SlotType,
) -> Result<PciPath, ServerSpecBuilderError> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::Nic if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x8, 0),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x10, 0),
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => PciPath::new(0, slot.0 + 0x18, 0),
        _ => return Err(ServerSpecBuilderError::PciSlotInvalid(slot.0, ty)),
    }
    .map_err(|_| ServerSpecBuilderError::PciSlotInvalid(slot.0, ty))
}
