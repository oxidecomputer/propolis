// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use slog::{o, Drain};

use crate::block;
use crate::hw::ata::bits::Registers::*;
use crate::hw::ata::bits::*;
use crate::hw::ata::geometry::*;
use crate::hw::ata::*;

#[test]
fn ata_device_power_on_reset() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    assert!(!device.status().busy());
    assert!(!device.status().error());
    assert!(device.status().device_ready());
    assert!(device.interrupts_enabled());

    // Assert the device returns the ATA device signature.
    assert_eq!(device.read_register(SectorCount), 0x1);
    assert_eq!(device.read_register(LbaLow), 0x1);
    assert_eq!(device.read_register(LbaMid), 0x0);
    assert_eq!(device.read_register(LbaHigh), 0x0);
}

#[test]
fn ata_device_unknown_command_code() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    device.write_register(Command, 0xff);
    assert!(command_aborted(&device));
}

#[test]
fn ata_device_unsupported_command() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    device.write_register(Command, Commands::DownloadMicrocode.into());
    assert!(command_aborted(&device))
}

#[test]
fn ata_device_identify() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    device.write_register(Command, Commands::IdenfityDevice.into());

    assert!(device.interrupt_pending());
    assert!(!&device.status().error());
    assert!(&device.status().data_request());

    let identity = read_sector(&device);
    assert!(command_success(&device));
    assert!(&device.status().device_ready());

    // Probe the IDENTITY for the correct geometry.
    assert_eq!(identity[1], GEOMETRY_300_SECTORS.cylinders);
    assert_eq!(identity[3], GEOMETRY_300_SECTORS.heads as u16);
    assert_eq!(identity[6], GEOMETRY_300_SECTORS.sectors as u16);
    assert_eq!(identity[57], 300);
    assert_eq!(identity[58], 0);
    assert_eq!(identity[60], 300);
    assert_eq!(identity[61], 0);
    assert_eq!(identity[100], 300);
    assert_eq!(identity[101], 0);
    assert_eq!(identity[102], 0);
}

#[test]
fn ata_device_clear_interrupt() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    // Trigger an interrupt.
    device.write_register(Command, Commands::IdenfityDevice.into());
    assert!(device.interrupt_pending());

    // Read the Alt Status register, which should not clear the interrupt.
    device.alt_status();
    assert!(device.interrupt_pending());

    // Read the Status register, clearing the interrupt as a side-effect.
    device.status();
    assert!(!device.interrupt_pending());
}

#[test]
fn ata_device_set_pio_mode() {
    let (device, _) = build_ata_device_with_300_colored_sectors();
    // Assert PIO mode 0.
    assert_eq!(read_identity(&device)[51] >> 8, 0x0u16);

    for mode in 0..5 {
        set_pio_mode(&device, mode);
        assert!(command_success(&device));
        // Assert the PIO mode set.
        assert_eq!(read_identity(&device)[51] >> 8, mode as u16);
    }
}

#[test]
fn ata_device_initialize_device_parameters() {
    let (device, _) = build_ata_device_with_300_colored_sectors();
    let geometry = Geometry { cylinders: 3, heads: 4, sectors: 25 };

    device.write_register(SectorCount, geometry.sectors);
    device.write_register(Device, geometry.heads - 1);
    device.write_register(Command, Commands::InitializeDeviceParameters.into());

    assert!(command_success(&device));
    assert!(device.interrupt_pending());

    let identity = read_identity(&device);
    assert_eq!(identity[54], geometry.cylinders);
    assert_eq!(identity[55], geometry.heads as u16);
    assert_eq!(identity[56], geometry.sectors as u16);
    assert_eq!(identity[57], 300);
    assert_eq!(identity[58], 0);
}

#[test]
fn ata_device_set_multiple_mode() {
    let (device, _) = build_ata_device_with_300_colored_sectors();

    device.write_register(SectorCount, 1);
    device.write_register(Command, Commands::SetMultipleMode.into());

    assert!(command_success(&device));
}

//
// Helpers
//

const GEOMETRY_300_SECTORS: Geometry =
    Geometry { cylinders: 2, heads: 3, sectors: 50 };

fn build_log() -> slog::Logger {
    let decorator = slog_term::PlainSyncDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();

    slog::Logger::root(drain, o!())
}

fn allocate_colored_sectors(sectors: usize) -> Vec<u8> {
    let mut memory: Vec<u8> = Vec::with_capacity(sectors * BLOCK_SIZE);

    for sector in 0..sectors {
        for _offset in 0..BLOCK_SIZE {
            memory.push(sector as u8);
        }
    }

    memory
}

fn build_ata_device_with_300_colored_sectors(
) -> (Arc<AtaDevice>, Arc<block::InMemoryBackend>) {
    let ata_device = Arc::new(AtaDevice::create(
        build_log(),
        GEOMETRY_300_SECTORS.capacity(),
        GEOMETRY_300_SECTORS,
    ));
    let backend = block::InMemoryBackend::create(
        allocate_colored_sectors(300),
        true,
        BLOCK_SIZE,
    ).unwrap();

    //backend.attach(ata_device.clone());
    (ata_device, backend)
}

fn command_success(device: &AtaDevice) -> bool {
    !device.alt_status().busy()
        && device.alt_status().device_ready()
        && !device.alt_status().error()
        && device.error().0 == 0x0
}

fn command_aborted(device: &AtaDevice) -> bool {
    !device.alt_status().busy()
        && device.alt_status().device_ready()
        && device.alt_status().error()
        && device.error().abort()
}

fn read_sector(device: &AtaDevice) -> [u16; 256] {
    let mut sector = [0u16; 256];

    for word in &mut sector[0..] {
        *word = device.read_data();
    }

    sector
}

fn read_identity(device: &AtaDevice) -> [u16; 256] {
    device.write_register(Command, Commands::IdenfityDevice.into());
    read_sector(device)
}

fn set_pio_mode(device: &AtaDevice, mode: u8) {
    device.write_register(SectorCount, 1 << 3 | mode);
    device.write_register(Features, 3);
    device.write_register(Command, Commands::SetFeatures.into());
}
