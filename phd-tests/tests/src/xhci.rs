// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::*;
use propolis_client::instance_spec::{
    PciPath, SpecKey, UsbDevice, XhciController,
};

#[phd_testcase]
async fn usb_device_enumerates(ctx: &Framework) {
    let mut config = ctx.vm_config_builder("xhci_usb_device_enumerates_test");
    let xhc_name: SpecKey = "xhc0".into();
    const PCI_DEV: u8 = 3;
    const USB_PORT: u8 = 2;
    config
        .xhci_controller(
            xhc_name.to_owned(),
            XhciController { pci_path: PciPath::new(0, PCI_DEV, 0)? },
        )
        .usb_device(UsbDevice {
            xhc_device: xhc_name,
            root_hub_port_num: USB_PORT,
        });

    let spec = config.vm_spec(ctx).await?;

    let mut vm = ctx.spawn_vm_with_spec(spec, None).await?;
    if !vm.guest_os_kind().is_linux() {
        phd_skip!("xhci/usb test uses sysfs to enumerate devices");
    }

    vm.launch().await?;
    vm.wait_to_boot().await?;

    let output = vm
        .run_shell_command(
            &format!("cat /sys/devices/pci0000:00/0000:00:{PCI_DEV:02}.0/usb1/1-{USB_PORT}/idVendor"),
        )
        .await?;
    // it's a device provided by us (0x1de)!
    assert_eq!(output, "01de");
}
