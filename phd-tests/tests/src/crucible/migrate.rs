// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use phd_testcase::*;
use tracing::info;
use uuid::Uuid;

#[phd_testcase]
fn smoke_test(ctx: &TestContext) {
    let mut config = ctx.vm_config_builder("crucible_migrate_smoke_source");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut source = ctx.spawn_vm(&config, None)?;
    let disk_handles = source.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);

    source.launch()?;
    source.wait_to_boot()?;

    let lsout = source.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");
    source.run_shell_command("touch ./foo.bar")?;
    source.run_shell_command("sync ./foo.bar")?;

    disk.set_generation(2);
    let mut target =
        ctx.spawn_successor_vm("crucible_migrate_smoke_target", &source, None)?;

    target.migrate_from(&source, Uuid::new_v4(), Duration::from_secs(60))?;
    let lsout = target.run_shell_command("ls foo.bar")?;
    assert_eq!(lsout, "foo.bar");
}

#[phd_testcase]
fn load_test(ctx: &TestContext) {
    let mut config = ctx.vm_config_builder("crucible_load_test_source");
    super::add_default_boot_disk(ctx, &mut config)?;
    let mut source = ctx.spawn_vm(&config, None)?;
    let disk_handles = source.cloned_disk_handles();
    let disk = disk_handles[0].as_crucible().unwrap();
    disk.set_generation(1);

    source.launch()?;
    source.wait_to_boot()?;

    disk.set_generation(2);
    let mut target =
        ctx.spawn_successor_vm("crucible_load_test_target", &source, None)?;

    // Create some random data.
    let block_count = 10;
    let ddout = source.run_shell_command(
        format!("dd if=/dev/random of=./rand.txt bs=5M count={}", block_count)
            .as_str(),
    )?;
    assert!(ddout.contains(format!("{}+0 records in", block_count).as_str()));

    // Compute the data's hash.
    let sha256sum_out = source.run_shell_command("sha256sum rand.txt")?;
    let checksum = sha256sum_out.split_whitespace().next().unwrap();
    info!("Generated SHA256 checksum: {}", checksum);

    // Start copying the generated file into a second file, then start a
    // migration while that copy is in progress.
    source.run_shell_command("dd if=./rand.txt of=./rand_new.txt &")?;
    target.migrate_from(&source, Uuid::new_v4(), Duration::from_secs(60))?;

    // Wait for the background command to finish running, then compute the
    // hash of the copied file. If all went well this will match the hash of
    // the source file.
    target.run_shell_command("wait $!")?;
    let sha256sum_target =
        target.run_shell_command("sha256sum rand_new.txt")?;
    let checksum_target = sha256sum_target.split_whitespace().next().unwrap();
    assert_eq!(checksum, checksum_target);
}
