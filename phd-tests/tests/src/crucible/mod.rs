use std::sync::Arc;

use phd_testcase::{
    phd_framework::disk::{
        crucible::CrucibleDisk, BlockSize, DiskError, DiskSource,
    },
    phd_skip, TestContext,
};

mod smoke;

/// Attempts to create a Crucible disk with the specified parameters. If the
/// runner did not specify a Crucible downstairs path, produces the special
/// "test skipped" error status for the caller to return.
fn create_disk_or_skip(
    ctx: &TestContext,
    source: DiskSource,
    disk_size_gib: u64,
    block_size: BlockSize,
) -> phd_testcase::Result<Arc<CrucibleDisk>> {
    let disk = ctx.disk_factory.create_crucible_disk(
        source,
        disk_size_gib,
        block_size,
    );

    if let Err(DiskError::NoCrucibleDownstairsPath) = disk {
        phd_skip!("Crucible binary not specified, can't create crucible disk");
    }

    Ok(disk?)
}

/// Creates a boot disk of the supplied size using the default guest image
/// artifact and a 512-byte block size. Returns the special "test skipped" error
/// status if Crucible is not enabled for this test run (see
/// [`create_disk_or_skip`]).
fn create_default_boot_disk(
    ctx: &TestContext,
    disk_size_gib: u64,
) -> phd_testcase::Result<Arc<CrucibleDisk>> {
    create_disk_or_skip(
        ctx,
        DiskSource::Artifact(&ctx.default_guest_image_artifact),
        disk_size_gib,
        BlockSize::Bytes512,
    )
}
