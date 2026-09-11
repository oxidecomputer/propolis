// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that primarily exercise the PHD framework itself.

use phd_framework::guest_os::GuestOsKind;
use phd_testcase::*;

#[phd_testcase]
async fn multiline_serial_test(ctx: &TestCtx) {
    let mut vm = ctx.spawn_default_vm("multiline_test").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let out = vm.run_shell_command("echo \\\nhello \\\nworld").await?;
    assert_eq!(out, "hello world");
}

#[phd_testcase]
async fn long_line_serial_test(ctx: &TestCtx) {
    let os = ctx.default_guest_os_kind().await?;
    if matches!(
        os,
        GuestOsKind::WindowsServer2016 | GuestOsKind::WindowsServer2019
    ) {
        phd_skip!(format!(
            "long serial lines not supported for guest OS {os:?}"
        ));
    }

    let mut vm = ctx.spawn_default_vm("long_line_serial_test").await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let long_str = "In my younger and more vulnerable years my father gave \
    me some advice that I've been turning over in my mind ever since. \
    \"Whenever you feel like sending a long serial console line,\" he told me, \
    \"just remember that all the guest OSes in this world haven't had the tty \
    settings you've had.\"";

    let out = vm
        .run_shell_command(&format!(
            "echo '{}'",
            // Fitzgerald didn't have to deal with nested Bash quotes, but this
            // test does. Replace apostrophes in the input string with a
            // string-terminating `'`, followed by an escaped single quote that
            // serves as the apostrophe, followed by a string-opening `'`.
            long_str.replace("'", "'\\''")
        ))
        .await?;
    assert_eq!(out, long_str);
}
