// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::*;
use tracing::info;

#[phd_testcase]
async fn acpi_tables_generation(ctx: &TestCtx) {
    let mut vm = ctx
        .spawn_vm(&ctx.vm_config_builder("acpi_tables_generation"), None)
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

        assert_eq!(out, creator_id_hex);
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

#[phd_testcase]
async fn acpi_tables_parse(ctx: &TestCtx) {
    let mut vm = ctx
        .spawn_vm(
            ctx.vm_config_builder("acpi_tables_parse").cpus(2), // Ensure fwts results are consistent.
            None,
        )
        .await?;

    if !vm.guest_os_kind().is_linux() {
        phd_skip!("requires Linux");
    }

    vm.launch().await?;
    vm.wait_to_boot().await?;

    // Skip test if guest doesn't have the necessary tools installed.
    let required_tools = ["acpidump", "iasl", "fwts"];
    for tool in required_tools.iter() {
        if vm.run_shell_command(&format!("which {}", tool)).await.is_err() {
            phd_skip!(format!("guest doesn't have {} installed", tool));
        }
    }

    let expected_files = ["apic", "dsdt", "facp", "facs", "ssdt"];

    // Verify we can dump the expected tables.
    vm.run_shell_command("acpidump -b").await.expect("acpidump");

    let ls = vm.run_shell_command("ls *.dat").await?;
    for file in expected_files.iter() {
        let expect = format!("{}.dat", file);
        assert!(ls.contains(&expect), "expected file {} to exist", expect);
    }

    // Verify ACPI tables can be parsed and disassembled.
    for file in expected_files.iter() {
        vm.run_shell_command(&format!("iasl -we -d {}.dat", file))
            .await
            .unwrap_or_else(|_| panic!("failed to disassemble {}.dat", file));

        let expect = format!("{}.dsl", file);
        let ls = vm.run_shell_command(&format!("ls {}", expect)).await?;
        assert!(ls.contains(&expect), "expected file {} to exist", expect);
    }

    // Verify fwts results.
    vm.run_shell_command("fwts --acpicompliance --acpitests; true").await?;
    let fwts_results: Vec<_> = vm
        .run_shell_command("tail -n 2 results.log | head -n 1")
        .await?
        .split('|')
        .map(|s| s.replace(" ", ""))
        .collect();

    // XXX(acpi): The current ACPI tables generate (num_cpus + 2) errors and 1
    //            warning.
    //
    // Test Failure Summary
    // ================================================================================
    //
    // Critical failures: NONE
    //
    // High failures: 1
    //  fadt: FADT X_GPE0_BLK Access width 0x00 but it should be 1 (byte access).
    //
    // Medium failures: 3
    //  madt: LAPIC has no matching processor UID 0
    //  madt: LAPIC has no matching processor UID 1
    //  madt: LAPICNMI has no matching processor UID 255
    //
    // Low failures: NONE
    //
    // Other failures: NONE
    let expexted_fwts_results = ["", "", "4", "0", "1"];
    assert_eq!(
        fwts_results[2], expexted_fwts_results[2],
        "expected {} fwts failures, got {}",
        expexted_fwts_results[2], fwts_results[2],
    );
    assert_eq!(
        fwts_results[3], expexted_fwts_results[3],
        "expected {} fwts aborts, got {}",
        expexted_fwts_results[3], fwts_results[3]
    );
    assert_eq!(
        fwts_results[4], expexted_fwts_results[4],
        "expected {} fwts warnings, got {}",
        expexted_fwts_results[4], fwts_results[4],
    );
}
