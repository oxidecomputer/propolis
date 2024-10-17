// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::lifecycle::Action;
use phd_testcase::*;

#[phd_testcase]
async fn lspci_lifecycle_test(ctx: &Framework) {
    const LSPCI: &str = "sudo lspci -vvx";
    const LSHW: &str = "sudo lshw -notime";

    let mut vm = ctx
        .spawn_vm(&ctx.vm_config_builder("lspci_lifecycle_test"), None)
        .await?;

    vm.launch().await?;
    vm.wait_to_boot().await?;

    // XXX: do not `ignore_status()` on these commands! They fail for any number
    // of reasons on different guests:
    // * sudo may not exist (some Alpine)
    // * lshw may not exist (Debian)
    // * we may not input a sudo password (Ubuntu)

    let lspci = vm.run_shell_command(LSPCI).await?.ignore_status();
    let lshw = vm.run_shell_command(LSHW).await?.ignore_status();
    ctx.lifecycle_test(vm, &[Action::StopAndStart], move |vm| {
        let lspci = lspci.clone();
        let lshw = lshw.clone();
        Box::pin(async move {
            let new_lspci =
                vm.run_shell_command(LSPCI).await.unwrap().ignore_status();
            assert_eq!(new_lspci, lspci);
            let new_lshw =
                vm.run_shell_command(LSHW).await.unwrap().ignore_status();
            assert_eq!(new_lshw, lshw);
        })
    })
    .await?;
}
