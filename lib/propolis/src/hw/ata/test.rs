// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;
use std::sync::Mutex;

use slog::{o, Drain};

use crate::block;
use crate::block::Backend;
use crate::hw::ata::bits::Commands::*;
use crate::hw::ata::bits::Registers::*;
use crate::hw::ata::bits::*;
use crate::hw::ata::controller::{AtaControllerState, ChannelSelect::*};
use crate::hw::ata::geometry::*;
use crate::hw::ata::AtaBlockDevice;
use crate::hw::pci;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_power_on_reset() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    let status = StatusRegister(controller.read_register(Primary, Status));
    assert!(!status.busy());
    assert!(!status.error());
    assert!(status.device_ready());
    //assert!(device.interrupts_enabled());

    // Assert the device returns the ATA device signature.
    assert_eq!(controller.read_register(Primary, SectorCount), 0x1);
    assert_eq!(controller.read_register(Primary, LbaLow), 0x1);
    assert_eq!(controller.read_register(Primary, LbaMid), 0x0);
    assert_eq!(controller.read_register(Primary, LbaHigh), 0x0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_unknown_command_code() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    controller.write_register(Primary, Command, 0xff);
    assert!(command_aborted(&mut controller));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_unsupported_command() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    controller.write_register(Primary, Command, DownloadMicrocode.into());
    assert!(command_aborted(&mut controller))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_identify() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    controller.write_register(Primary, Command, IdenfityDevice.into());
    assert!(command_success(&mut controller));
    assert!(StatusRegister(controller.read_register(Primary, AltStatus))
        .device_ready());

    // assert!(device.interrupt_pending());
    let status = StatusRegister(controller.read_register(Primary, Status));
    assert!(!status.error());
    assert!(status.data_request());

    // Probe the IDENTITY for the correct geometry.
    let identity = read_sector(&mut controller);
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

// #[test]
// fn ata_device_clear_interrupt() {
//     let mut device = build_ata_device();

//     // Trigger an interrupt.
//     device.write_register(Command, Commands::IdenfityDevice.into());
//     assert!(device.interrupt_pending());

//     // Read the Alt Status register, which should not clear the interrupt.
//     device.alt_status();
//     assert!(device.interrupt_pending());

//     // Read the Status register, clearing the interrupt as a side-effect.
//     device.status();
//     assert!(!device.interrupt_pending());
// }

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_set_pio_mode() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    // Assert PIO mode 0.
    assert_eq!(read_identity(&mut controller)[51] >> 8, 0x0u16);

    for mode in 0..5 {
        set_pio_mode(&mut controller, mode);
        assert!(command_success(&mut controller));
        // Assert the PIO mode set.
        assert_eq!(read_identity(&mut controller)[51] >> 8, mode as u16);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_initialize_device_parameters() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    let geometry = Geometry { cylinders: 3, heads: 4, sectors: 25 };
    controller.write_register(Primary, SectorCount, geometry.sectors);
    controller.write_register(Primary, Device, geometry.heads - 1);
    controller.write_register(
        Primary,
        Command,
        InitializeDeviceParameters.into(),
    );

    assert!(command_success(&mut controller));
    //assert!(device.interrupt_pending());

    let identity = read_identity(&mut controller);
    assert_eq!(identity[54], geometry.cylinders);
    assert_eq!(identity[55], geometry.heads as u16);
    assert_eq!(identity[56], geometry.sectors as u16);
    assert_eq!(identity[57], 300);
    assert_eq!(identity[58], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ata_device_set_multiple_mode() {
    let (controller, _) = build_ata_controller_and_block_device();
    let mut controller = controller.lock().unwrap();

    controller.write_register(Primary, SectorCount, 1);
    controller.write_register(Primary, Command, SetMultipleMode.into());

    assert!(command_success(&mut controller));
}

//
// Helpers
//

type AtaControllerPtr = Arc<Mutex<AtaControllerState>>;
type LockedController<'a> = std::sync::MutexGuard<'a, AtaControllerState>;

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

fn build_ata_controller_and_block_device(
) -> (AtaControllerPtr, Arc<block::InMemoryBackend>) {
    let pci_state = Arc::new(pci::Builder::new(pci::Ident::default()).finish());
    let ata_controller = Arc::new(Mutex::new(AtaControllerState::new()));
    let ata_block_device =
        AtaBlockDevice::new(&pci_state, &ata_controller, Primary, 0);
    let block_backend = block::InMemoryBackend::create(
        allocate_colored_sectors(300),
        false,
        BLOCK_SIZE,
    )
    .unwrap();

    block_backend
        .attach(ata_block_device.clone() as Arc<dyn block::Device>)
        .unwrap();
    ata_controller
        .lock()
        .unwrap()
        .attach_device(
            build_log(),
            &ata_block_device,
            GEOMETRY_300_SECTORS.capacity(),
            GEOMETRY_300_SECTORS,
        )
        .unwrap();

    (ata_controller, block_backend)
}

fn command_success(controller: &mut LockedController) -> bool {
    let alt_status = StatusRegister(controller.read_register(Primary, Status));
    let error = ErrorRegister(controller.read_register(Primary, Error));

    !alt_status.busy()
        && alt_status.device_ready()
        && !alt_status.error()
        && error.0 == 0x0
}

fn command_aborted(controller: &mut LockedController) -> bool {
    let alt_status = StatusRegister(controller.read_register(Primary, Status));
    let error = ErrorRegister(controller.read_register(Primary, Error));

    !alt_status.busy()
        && alt_status.device_ready()
        && alt_status.error()
        && error.abort()
}

fn read_sector(controller: &mut LockedController) -> [u16; 256] {
    let mut sector = [0u16; 256];

    for word in &mut sector[0..] {
        *word = controller.read_data16(Primary);
    }

    sector
}

fn read_identity(controller: &mut LockedController) -> [u16; 256] {
    controller.write_register(Primary, Command, IdenfityDevice.into());
    read_sector(controller)
}

fn set_pio_mode(controller: &mut LockedController, mode: u8) {
    controller.write_register(Primary, SectorCount, 1 << 3 | mode);
    controller.write_register(Primary, Features, 3);
    controller.write_register(Primary, Command, SetFeatures.into());
}
