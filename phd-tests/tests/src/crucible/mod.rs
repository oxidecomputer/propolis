// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::{
    phd_framework::{
        disk::BlockSize,
        test_vm::{DiskBackend, DiskInterface, VmConfig},
    },
    *,
};

mod migrate;
mod smoke;

fn add_crucible_boot_disk_or_skip<'a>(
    ctx: &Framework,
    config: &mut VmConfig<'a>,
    artifact: &'a str,
    interface: DiskInterface,
    pci_slot: u8,
    min_disk_size_gib: u64,
    block_size: BlockSize,
) -> phd_testcase::Result<()> {
    if !ctx.crucible_enabled() {
        phd_skip!("Crucible backends not enabled (no downstairs path)");
    }

    config.boot_disk(
        artifact,
        interface,
        DiskBackend::Crucible { min_disk_size_gib, block_size },
        pci_slot,
    );

    Ok(())
}

fn add_default_boot_disk<'a>(
    ctx: &'a Framework,
    config: &mut VmConfig<'a>,
) -> phd_testcase::Result<()> {
    add_crucible_boot_disk_or_skip(
        ctx,
        config,
        ctx.default_guest_os_artifact(),
        DiskInterface::Nvme,
        4,
        10,
        BlockSize::Bytes512,
    )
}
