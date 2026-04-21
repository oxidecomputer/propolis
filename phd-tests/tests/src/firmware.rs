// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use acpi_tables;
use phd_testcase::*;
use tracing::info;

#[phd_testcase]
async fn acpi_tables_generation(ctx: &Framework) {
    let mut vm = ctx
        .spawn_vm(&ctx.vm_config_builder("lspci_lifecycle_test"), None)
        .await?;

    if !vm.guest_os_kind().is_linux() {
        phd_skip!("requires Linux");
    }

    vm.launch().await?;
    vm.wait_to_boot().await?;

    const ACPI_TABLES_PATH: &str = "/sys/firmware/acpi/tables";

    // Verify ACPI tables have the expected Creator ID value.
    let creator_id_hex = acpi_tables::CREATOR_ID
        .iter()
        .map(|i| format!("{:02x?}", i))
        .collect::<String>();

    struct CreatorIdTestCase<'a> {
        table: &'a str,
        offset: u8,
        len: u8,
    }

    let creator_id_test_cases = [
        CreatorIdTestCase { table: "APIC", offset: 28, len: 4 },
        CreatorIdTestCase { table: "DSDT", offset: 28, len: 4 },
        CreatorIdTestCase { table: "FACP", offset: 28, len: 4 },
        CreatorIdTestCase { table: "SSDT", offset: 28, len: 4 },
    ];
    for case in creator_id_test_cases.iter() {
        let cmd = format!(
            "xxd -s {1} -l {2} -c {2} -p {3}/{0}",
            case.table, case.offset, case.len, ACPI_TABLES_PATH,
        );
        let out = vm.run_shell_command(&cmd).await?;
        info!(out, "{} creator ID", case.table);

        assert!(out.contains(&creator_id_hex));
    }

    // Verify FACS table have the expected version.
    // The generated FACS table has version equal to 1.
    const FACS_VERSION_OFFSET: u8 = 32;
    const FACS_VERSION_LEN: u8 = 1;
    let facs_version_cmd = format!(
        "xxd -s {1} -l {2} -c {2} -p {3}/{0}",
        "FACS", FACS_VERSION_OFFSET, FACS_VERSION_LEN, ACPI_TABLES_PATH
    );
    let facs_version = vm.run_shell_command(&facs_version_cmd).await?;
    info!(facs_version, "FACS table version");
    assert_eq!(facs_version, "01");
}
