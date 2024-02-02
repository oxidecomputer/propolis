// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use async_trait::async_trait;
use phd_framework::lifecycle::Action;
use phd_testcase::{
    phd_framework::{lifecycle::CheckVm, TestVm},
    *,
};

const LSPCI: &str = "sudo lspci -vvx";
const LSHW: &str = "sudo lshw -notime";

#[phd_testcase]
async fn lspci_lifecycle_test(ctx: &Framework) {
    let mut vm = ctx
        .spawn_vm(&ctx.vm_config_builder("lspci_lifecycle_test"), None)
        .await?;

    vm.launch().await?;
    vm.wait_to_boot().await?;
    struct Check {
        lspci: String,
        lshw: String,
    }

    #[async_trait]
    impl CheckVm for Check {
        async fn check(&self, vm: &TestVm) {
            let new_lspci = vm.run_shell_command(LSPCI).await.unwrap();
            assert_eq!(new_lspci, self.lspci);
            let new_lshw = vm.run_shell_command(LSHW).await.unwrap();
            assert_eq!(new_lshw, self.lshw);
        }
    }

    let check = Check {
        lspci: vm.run_shell_command(LSPCI).await?,
        lshw: vm.run_shell_command(LSHW).await?,
    };
    ctx.lifecycle_test(vm, &[Action::StopAndStart], check).await?;
}
