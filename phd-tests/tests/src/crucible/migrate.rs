// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::test_vm::MigrationTimeout;
use phd_testcase::*;
use tracing::info;
use uuid::Uuid;

#[phd_testcase]
async fn smoke_test(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("crucible_migrate_smoke_source");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut source = ctx.spawn_vm(&config, None).await?;
    let disk_handles = source.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);

    source.launch().await?;
    source.wait_to_boot().await?;

    let lsout =
        source.run_shell_command("ls foo.bar 2> /dev/null").check_err().await?;
    assert_eq!(lsout, "");
    source.run_shell_command("touch ./foo.bar").await?;
    source.run_shell_command("sync ./foo.bar").await?;

    disk.set_generation(2);
    let mut target = ctx
        .spawn_successor_vm("crucible_migrate_smoke_target", &source, None)
        .await?;

    target
        .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
        .await?;
    let lsout = target.run_shell_command("ls foo.bar").await?;
    assert_eq!(lsout, "foo.bar");
}

#[phd_testcase]
async fn load_test(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("crucible_load_test_source");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut source = ctx.spawn_vm(&config, None).await?;
    let disk_handles = source.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);

    source.launch().await?;
    source.wait_to_boot().await?;

    disk.set_generation(2);
    let mut target = ctx
        .spawn_successor_vm("crucible_load_test_target", &source, None)
        .await?;

    // Create some random data.
    let block_count = 10;
    let ddout = source
        .run_shell_command(
            format!(
                "dd if=/dev/random of=./rand.txt bs=5M count={}",
                block_count
            )
            .as_str(),
        )
        .await?;
    assert!(ddout.contains(format!("{}+0 records in", block_count).as_str()));

    // Compute the data's hash.
    let sha256sum_out = source.run_shell_command("sha256sum rand.txt").await?;
    let checksum = sha256sum_out.split_whitespace().next().unwrap();
    info!("Generated SHA256 checksum: {}", checksum);

    // Start copying the generated file into a second file, then start a
    // migration while that copy is in progress.
    source.run_shell_command("dd if=./rand.txt of=./rand_new.txt &").await?;
    target
        .migrate_from(&source, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    // Wait for the background command to finish running, then compute the
    // hash of the copied file. If all went well this will match the hash of
    // the source file.
    target.run_shell_command("wait $!").await?;
    let sha256sum_target =
        target.run_shell_command("sha256sum rand_new.txt").await?;
    let checksum_target = sha256sum_target.split_whitespace().next().unwrap();
    assert_eq!(checksum, checksum_target);
}
