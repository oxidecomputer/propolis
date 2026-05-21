// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use phd_testcase::*;

// This test verifies that the ACPI tables generated for a VM match the tables
// we expect to find.
//
// The test tables are read from versioned directories in  `testdata/acpi`. Use
// `propolis-server` with the `acpi-debug` feature enabled and create a VM with
// 2 vCPUs to generate new reference tables.
//
// ```
// cargo build --bin propolis-server --features acpi-debug
// ```
//
// `propolis-standalone` creates devices with slightly different names, so the
// tables it generates may not match the ones generated in PHD tests. Always
// use `propolis-server` to create reference tables.
//
// The generated ACPI tables will be written to a directory called `acpi` in
// the working directory in which `propolis-server` is running. You can copy
// them over to `testdata/acpi`.
//
// To debug test failures, feed the hex ACPI table representation from the
// failed assertion message to `xxd -r -p` and save the output to a file.
//
// ```
// $ cargo xtask phd run
// ...
// assertion `left == right` failed: expected FACP table to match
//  left: "46414350f400000003..."
//  right: "46414350f400000003..."
// ...
//
// $ echo '46414350f400000003...' | xxd -r -p > facp.dat
// ```
//
// You can compare the binary file directly against the expected file in
// `testdata/acpi` or decompile the AML code into ASL using the `iasl` tool
// from the ACPICA project.
//
// ```
// iasl -d facp.dat
// ```
#[phd_testcase]
async fn acpi_tables_generation(ctx: &TestCtx) {
    use propolis::firmware::acpi;
    use std::fmt::Write;

    let mut vm = ctx
        .spawn_vm(ctx.vm_config_builder("acpi_tables_generation").cpus(2), None)
        .await?;

    if !vm.guest_os_kind().is_linux() {
        phd_skip!("requires Linux");
    }

    vm.launch().await?;
    vm.wait_to_boot().await?;

    const ACPI_TABLES_PATH: &str = "/sys/firmware/acpi/tables";

    // These are the tables that Linux makes available in the /sys path above.
    //
    // We don't need to check the RSDP and XSDT tables because OVMF always
    // overwrites them with its own version. OVMF also introduces new tables,
    // such as BGRT, and we don't need to test them either.
    let expected_madt = include_bytes!("../testdata/acpi/v0/madt.dat").to_vec();
    let expected_dsdt = include_bytes!("../testdata/acpi/v0/dsdt.dat").to_vec();
    let expected_fadt = include_bytes!("../testdata/acpi/v0/fadt.dat").to_vec();
    let expected_ssdt = include_bytes!("../testdata/acpi/v0/ssdt.dat").to_vec();

    // TablePatch represents a range that will be copied over from the expected
    // table. These usually represent runtime values that can change at
    // runtime, such as addresses to other tables and checksums.
    struct TablePatch {
        offset: usize,
        length: usize,
    }

    struct TestCase<'a> {
        table: &'a str,
        expect: Vec<u8>,
        patches: Vec<TablePatch>,
    }

    for case in [
        TestCase { table: "APIC", expect: expected_madt, patches: vec![] },
        TestCase { table: "DSDT", expect: expected_dsdt, patches: vec![] },
        TestCase {
            table: "FACP",
            expect: expected_fadt,
            patches: vec![
                TablePatch {
                    offset: acpi::TABLE_HEADER_CHECKSUM_OFFSET,
                    length: acpi::TABLE_HEADER_CHECKSUM_LEN,
                },
                TablePatch {
                    offset: acpi::FADT_FACS_OFFSET,
                    length: acpi::FADT_FACS_LEN,
                },
                TablePatch {
                    offset: acpi::FADT_DSDT_OFFSET,
                    length: acpi::FADT_DSDT_LEN,
                },
                TablePatch {
                    offset: acpi::FADT_X_DSDT_OFFSET,
                    length: acpi::FADT_X_DSDT_LEN,
                },
            ],
        },
        TestCase {
            table: "SSDT",
            expect: expected_ssdt,
            patches: vec![
                TablePatch {
                    offset: acpi::TABLE_HEADER_CHECKSUM_OFFSET,
                    length: acpi::TABLE_HEADER_CHECKSUM_LEN,
                },
                TablePatch {
                    offset: acpi::SSDT_FWDT_ADDR_OFFSET,
                    length: acpi::SSDT_FWDT_ADDR_LEN,
                },
            ],
        },
    ] {
        let cmd = format!("xxd -p -c0 {0}/{1}", ACPI_TABLES_PATH, case.table);
        let mut out: String =
            vm.run_shell_command(&cmd).await?.split_whitespace().collect();

        let mut expected_hex = String::new();
        for b in case.expect.iter() {
            write!(expected_hex, "{:02x}", b).unwrap();
        }

        for p in &case.patches {
            let start = p.offset * 2; // Each byte is represented by 2 characters.
            let end = start + p.length * 2;
            let r = start..end;
            out.replace_range(r.clone(), &expected_hex[r.clone()]);
        }

        assert_eq!(expected_hex, out, "expected {} table to match", case.table);
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
