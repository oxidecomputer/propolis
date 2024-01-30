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

    let lspci = vm.run_shell_command(LSPCI).await?;
    let lshw = vm.run_shell_command(LSHW).await?;
    ctx.lifecycle_test(
        vm,
        &[Action::StopAndStart],
        // REVIEW(gjc): the clone here is very ugly but was the most
        // straightforward way to convince the compiler that the lspci/lshw
        // outputs live long enough. There has to be a better way.
        move |vm| {
            let lspci = lspci.clone();
            let lshw = lshw.clone();
            Box::pin(async move {
                let new_lspci = vm.run_shell_command(LSPCI).await.unwrap();
                assert_eq!(new_lspci, lspci);
                let new_lshw = vm.run_shell_command(LSHW).await.unwrap();
                assert_eq!(new_lshw, lshw);
            })
        },
    )
    .await?;
}
