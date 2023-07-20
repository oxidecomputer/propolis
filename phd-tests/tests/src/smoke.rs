// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::{
    phd_framework::{disk::DiskSource, test_vm::vm_config::DiskInterface},
    *,
};

#[phd_testcase]
fn nproc_test(ctx: &TestContext) {
    let mut vm = ctx
        .vm_factory
        .new_vm("nproc_test", ctx.default_vm_config().set_cpus(6))?;
    vm.launch()?;
    vm.wait_to_boot()?;

    let nproc = vm.run_shell_command("nproc")?;
    assert_eq!(nproc.parse::<u8>().unwrap(), 6);
}

#[phd_testcase]
fn instance_spec_get_test(ctx: &TestContext) {
    let disk = ctx.disk_factory.create_file_backed_disk(
        DiskSource::Artifact(&ctx.default_guest_image_artifact),
    )?;

    let config = ctx
        .deviceless_vm_config()
        .set_cpus(4)
        .set_memory_mib(3072)
        .set_boot_disk(disk, 4, DiskInterface::Nvme);
    let mut vm = ctx.vm_factory.new_vm("instance_spec_test", config)?;
    vm.launch()?;

    let spec_get_response = vm.get_spec()?;
    let propolis_client::types::VersionedInstanceSpec::V0(spec) =
        spec_get_response.spec;
    assert_eq!(spec.devices.board.cpus, 4);
    assert_eq!(spec.devices.board.memory_mb, 3072);
}
