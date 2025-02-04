// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use cpuid_utils::{CpuidIdent, CpuidValues, CpuidVendor};
use phd_framework::test_vm::MigrationTimeout;
use phd_testcase::*;
use propolis_client::types::{
    CpuidEntry, InstanceSpecStatus, VersionedInstanceSpec,
};
use tracing::info;
use uuid::Uuid;

fn cpuid_entry(
    leaf: u32,
    subleaf: Option<u32>,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
) -> CpuidEntry {
    CpuidEntry { leaf, subleaf, eax, ebx, ecx, edx }
}

#[phd_testcase]
async fn cpuid_instance_spec_round_trip_test(ctx: &Framework) {
    // The guest isn't actually going to boot with these nonsense settings. The
    // goal is simply to verify that the ensure API properly records these
    // options and reflects them back out on request.
    let entries = vec![
        cpuid_entry(0, None, 0xaaaa, 0xbbbb, 0xcccc, 0xdddd),
        cpuid_entry(0x8000_0000, None, 0x88aa, 0x88bb, 0x88cc, 0x88dd),
    ];

    let mut cfg = ctx.vm_config_builder("cpuid_instance_spec_round_trip_test");
    cfg.cpuid(entries.clone());
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;

    let spec_get_response = vm.get_spec().await?;
    let InstanceSpecStatus::Present(VersionedInstanceSpec::V0(spec)) =
        spec_get_response.spec
    else {
        panic!("instance spec should be present for a running VM");
    };

    let cpuid = spec.board.cpuid.expect("board should have explicit CPUID");
    assert_eq!(cpuid.entries.len(), entries.len());
    itertools::assert_equal(cpuid.entries, entries);
}

#[phd_testcase]
async fn cpuid_boot_and_migrate_test(ctx: &Framework) {
    use cpuid_utils::bits::*;

    // Use the current host's CPUID values to try to make this test
    // host-agnostic.
    let mut host_cpuid = cpuid_utils::host::query_complete(
        cpuid_utils::host::CpuidSource::BhyveDefault,
    )?;

    info!(?host_cpuid, "read bhyve default CPUID");

    // Linux guests expect to see at least a couple of leaves in the extended
    // CPUID range. These have vendor-specific meanings. This test only encodes
    // AMD's definitions, so skip the test if leaf 0 reports any other vendor.
    if host_cpuid.vendor() != CpuidVendor::Amd {
        phd_skip!("cpuid_boot_test can only run on AMD hosts");
    }

    // This test works by injecting a fake brand string into extended leaves
    // 0x8000_0002-0x8000_0004 and seeing if the guest observes that string.
    //
    // The brand string leaves have been defined for long enough that they
    // should be present on virtually any host that's modern enough to run
    // Propolis and PHD. Fail the test (instead of skipping) if they're missing,
    // since that may indicate a latent bug in the `cpuid_utils` crate.
    let ext_leaf_0 = host_cpuid
        .get(CpuidIdent::leaf(EXTENDED_BASE_LEAF))
        .expect("PHD-capable processors should have some extended leaves");

    assert!(
        ext_leaf_0.eax >= 0x8000_0004,
        "PHD-capable processors should support at least leaf 0x8000_0004 \
        (reported {})",
        ext_leaf_0.eax
    );

    // Reprogram the brand string leaves and see if the new string shows up in
    // the guest.
    const BRAND_STRING: &[u8; 48] =
        b"Oxide Cloud Computer Company Cloud Computer\0\0\0\0\0";

    let chunks = BRAND_STRING.chunks_exact(4);
    let mut ext_leaf_2 = CpuidValues::default();
    let mut ext_leaf_3 = CpuidValues::default();
    let mut ext_leaf_4 = CpuidValues::default();
    let dst = ext_leaf_2
        .iter_mut()
        .chain(ext_leaf_3.iter_mut())
        .chain(ext_leaf_4.iter_mut());

    for (chunk, dst) in chunks.zip(dst) {
        *dst = u32::from_le_bytes(chunk.try_into().unwrap());
    }

    host_cpuid.insert(CpuidIdent::leaf(0x8000_0002), ext_leaf_2).unwrap();
    host_cpuid.insert(CpuidIdent::leaf(0x8000_0003), ext_leaf_3).unwrap();
    host_cpuid.insert(CpuidIdent::leaf(0x8000_0004), ext_leaf_4).unwrap();

    // Try to boot a guest with the computed CPUID values. The modified brand
    // string should show up in /proc/cpuinfo.
    let mut cfg = ctx.vm_config_builder("cpuid_boot_test");
    cfg.cpuid(
        host_cpuid
            .iter()
            .map(|(leaf, value)| CpuidEntry {
                leaf: leaf.leaf,
                subleaf: leaf.subleaf,
                eax: value.eax,
                ebx: value.ebx,
                ecx: value.ecx,
                edx: value.edx,
            })
            .collect(),
    );
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    let cpuinfo = vm.run_shell_command("cat /proc/cpuinfo").await?;
    info!(cpuinfo, "/proc/cpuinfo output");
    assert!(cpuinfo.contains(
        std::str::from_utf8(BRAND_STRING).unwrap().trim_matches('\0')
    ));

    // Migrate the VM and make sure the brand string setting persists.
    let mut target = ctx
        .spawn_successor_vm("cpuid_boot_test_migration_target", &vm, None)
        .await?;

    target
        .migrate_from(&vm, Uuid::new_v4(), MigrationTimeout::default())
        .await?;

    // Reset the target to force it to reread its CPU information.
    target.reset().await?;
    target.wait_to_boot().await?;
    let cpuinfo = target.run_shell_command("cat /proc/cpuinfo").await?;
    info!(cpuinfo, "/proc/cpuinfo output");
    assert!(cpuinfo.contains(
        std::str::from_utf8(BRAND_STRING).unwrap().trim_matches('\0')
    ));
}
