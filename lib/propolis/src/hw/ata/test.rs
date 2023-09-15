// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::hw::ata::bits::Registers::*;
use crate::hw::ata::bits::*;
use crate::hw::ata::geometry::*;
use crate::hw::ata::*;

use slog::{o, Drain};

#[test]
fn ata_device_power_on_reset() {
    let mut device = build_device_with_300_sectors();

    assert!(!status(&mut device).busy());
    assert!(!status(&mut device).error());
    assert!(status(&mut device).device_ready());
    assert!(device.interrupts_enabled());

    // Assert the device returns the ATA device signature.
    assert_eq!(device.read_register(SectorCount), 0x1);
    assert_eq!(device.read_register(LbaLow), 0x1);
    assert_eq!(device.read_register(LbaMid), 0x0);
    assert_eq!(device.read_register(LbaHigh), 0x0);
}

#[test]
fn ata_device_unknown_command_code() {
    let mut device = build_device_with_300_sectors();

    device.write_register(Command, 0xff);
    assert!(command_aborted(&mut device));
}

#[test]
fn ata_device_unsupported_command() {
    let mut device = build_device_with_300_sectors();

    device.write_register(Command, Commands::DownloadMicrocode.into());
    assert!(command_aborted(&mut device))
}

#[test]
fn ata_device_identify() {
    let mut device = build_device_with_300_sectors();

    device.write_register(Command, Commands::IdenfityDevice.into());

    assert!(device.interrupt_pending());
    assert!(!status(&mut device).error());
    assert!(status(&mut device).data_request());

    let identity = read_sector(&mut device);
    assert!(command_success(&mut device));
    assert!(status(&mut device).device_ready());

    // Probe for some expected words in the IDENTITY.
    assert_eq!(identity[1], GEOMETRY_300_SECTORS.cylinders);
    assert_eq!(identity[3], GEOMETRY_300_SECTORS.heads as u16);
    assert_eq!(identity[6], GEOMETRY_300_SECTORS.sectors as u16);
}

const GEOMETRY_300_SECTORS: Geometry =
    Geometry { cylinders: 2, heads: 3, sectors: 50 };

fn build_log() -> slog::Logger {
    let decorator = slog_term::PlainSyncDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();

    slog::Logger::root(drain, o!())
}

fn build_device_with_300_sectors() -> AtaDevice {
    AtaDevice::create(
        build_log(),
        GEOMETRY_300_SECTORS.capacity(),
        GEOMETRY_300_SECTORS,
    )
}

fn status(device: &mut AtaDevice) -> StatusRegister {
    StatusRegister(device.read_register(Status))
}

fn error(device: &mut AtaDevice) -> ErrorRegister {
    ErrorRegister(device.read_register(Error))
}

fn command_success(device: &mut AtaDevice) -> bool {
    !status(device).busy()
        && status(device).device_ready()
        && !status(device).error()
        && error(device).0 == 0x0
}

fn command_aborted(device: &mut AtaDevice) -> bool {
    !status(device).busy()
        && status(device).device_ready()
        && status(device).error()
        && error(device).abort()
}

fn read_sector(device: &mut AtaDevice) -> [u16; 256] {
    let mut sector = [0u16; 256];

    for word in &mut sector[0..] {
        *word = device.read_data();
    }

    sector
}
