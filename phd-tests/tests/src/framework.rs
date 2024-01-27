// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This module contains tests whose primary goal is to verify the correctness
//! of the PHD framework itself.

use phd_testcase::*;

#[phd_testcase]
fn multiline_serial_test(ctx: &Framework) {
    let mut vm = ctx.spawn_default_vm("multiline_test")?;
    vm.launch()?;
    vm.wait_to_boot()?;

    let out = vm.run_shell_command("echo \\\nhello \\\nworld")?;
    assert_eq!(out, "hello world");
}
