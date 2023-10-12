// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_framework::lifecycle::Action;
use phd_testcase::*;

#[phd_testcase]
fn lspci_lifecycle_test(ctx: &Framework) {
    const LSPCI: &str = "sudo lspci -vvx";
    const LSHW: &str = "sudo lshw -notime";

    let mut vm =
        ctx.spawn_vm(&ctx.vm_config_builder("lspci_lifecycle_test"), None)?;

    vm.launch()?;
    vm.wait_to_boot()?;

    let lspci = vm.run_shell_command(LSPCI)?;
    let lshw = vm.run_shell_command(LSHW)?;
    ctx.lifecycle_test(vm, &[Action::StopAndStart], |vm| {
        let new_lspci = vm.run_shell_command(LSPCI).unwrap();
        assert_eq!(new_lspci, lspci);
        let new_lshw = vm.run_shell_command(LSHW).unwrap();
        assert_eq!(new_lshw, lshw);
    })?;
}
