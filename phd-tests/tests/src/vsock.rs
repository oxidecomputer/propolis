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
    let mut cfg = ctx.vm_config_builder("vsock_get_cid");
    cfg.vsock(GUEST_CID, PCI_DEV_NUM);
    cfg.cpus(4);

    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    // TODO: Remove the dependency on python
    if vm.run_shell_command("test -e /usr/bin/python3").await.is_err() {
        phd_skip!("guest doesn't have python3 installed");
    }

    // We don't really want to have to deal with python in a test but this is
    // an easy way to get the cid until we support custom VM test images or
    // tool block devices.
    const GET_CID: &str = r#"python3 -c "import struct,fcntl;f=open('/dev/vsock','rb');print(struct.unpack('I',fcntl.ioctl(f,0x7b9,bytes(4)))[0])""#;

    let cid = vm.run_shell_command(GET_CID).await?.parse::<u64>()?;
    assert_eq!(cid, GUEST_CID, "guest cid matches what was configured");
}
