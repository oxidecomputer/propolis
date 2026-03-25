use phd_testcase::*;

const GUEST_CID: u64 = 16;
const PCI_DEV_NUM: u8 = 26;

#[phd_testcase]
async fn vsock_smoke_test(ctx: &TestCtx) {
    let mut cfg = ctx.vm_config_builder("vsock_smoke_test");
    cfg.vsock(GUEST_CID, PCI_DEV_NUM);
    cfg.cpus(4);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // This doesn't tell the whole story since linux will sometimes make this
    // device available even if the hypervisor does not present the virtio
    // device itself. Either way, it would be an error if it's not present.
    vm.run_shell_command("test -e /dev/vsock").await?;
}

#[phd_testcase]
async fn vsock_get_cid(ctx: &TestCtx) {
    const GET_CID: &str = "/usr/local/bin/getcid";

    let mut cfg = ctx.vm_config_builder("vsock_get_cid");
    cfg.vsock(GUEST_CID, PCI_DEV_NUM);
    cfg.cpus(4);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // If we are not using a modified alpine image with our additional tooling
    // we should skip this test entirely.
    if vm.run_shell_command(&format!("test -e {GET_CID}")).await.is_err() {
        phd_skip!("guest doesn't have getcid installed");
    }

    let cid = vm.run_shell_command(GET_CID).await?.parse::<u64>()?;
    assert_eq!(cid, GUEST_CID, "guest cid matches what was configured");
}
