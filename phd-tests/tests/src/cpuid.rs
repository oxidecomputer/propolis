// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use cpuid_utils::{CpuidIdent, CpuidValues, CpuidVendor};
use phd_testcase::*;
use propolis_client::types::CpuidEntry;
use tracing::info;

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
    let propolis_client::types::VersionedInstanceSpec::V0(spec) =
        spec_get_response.spec;

    let cpuid = spec.board.cpuid.expect("board should have explicit CPUID");
    assert_eq!(cpuid.entries.len(), entries.len());
    itertools::assert_equal(cpuid.entries, entries);
}

#[phd_testcase]
async fn cpuid_boot_test(ctx: &Framework) {
    use cpuid_utils::bits::*;

    // This test verifies that plumbing a reasonably sensible-looking set of
    // CPUID values into the guest produces a bootable guest.
    //
    // The definition of "reasonably sensible" is derived from the suggested
    // virtual-Milan CPUID definitions in RFD 314. Those are in turn derived
    // from reading AMD's manuals and bhyve's source code to figure out what
    // host CPU features to hide from guests.
    //
    // To try to make this test at least somewhat host-agnostic, read all of the
    // host's CPUID leaves, then filter to just a handful of leaves that
    // advertise features to the guest (and that Linux guests will check for
    // during boot).
    let mut host_cpuid = cpuid_utils::host_query_all().0;
    info!(?host_cpuid, "read host CPUID");
    let leaf_0_values = *host_cpuid
        .get(&CpuidIdent::leaf(0))
        .expect("host CPUID leaf 0 should always be present");

    // Linux guests expect to see at least a couple of leaves in the extended
    // CPUID range. These have vendor-specific meanings. This test only encodes
    // AMD's definitions, so skip the test if leaf 0 reports any other vendor.
    let Ok(CpuidVendor::Amd) = CpuidVendor::try_from(leaf_0_values) else {
        phd_skip!("cpuid_boot_test can only run on AMD hosts");
    };

    // Report up through leaf 7 to get the extended feature flags that are
    // defined there.
    if leaf_0_values.eax < 7 {
        phd_skip!(
            "cpuid_boot_test requires the host to report at least leaf 7"
        );
    }

    // Keep only standard leaves up to 7 and the first two extended leaves.
    // Extended leaves 2 through 4 will be overwritten with a fake brand string
    // (see below).
    host_cpuid.retain(|leaf, _values| {
        leaf.leaf <= 0x7
            || (leaf.leaf >= 0x8000_0000 && leaf.leaf <= 0x8000_0001)
    });

    // Report that leaf 7 is the last available standard leaf.
    host_cpuid.get_mut(&CpuidIdent::leaf(0)).unwrap().eax = 7;

    // Mask off bits that a minimum-viable Oxide platform doesn't support or
    // can't (or won't) expose to guests. See RFD 314 for further discussion of
    // how these masks were chosen.
    //
    // These are masks (and not assignments) so that, if the host processor
    // doesn't support a feature contained in an `ALL_FLAGS` value, the
    // feature will not be enabled in the guest.
    let leaf_1 = host_cpuid.get_mut(&CpuidIdent::leaf(1)).unwrap();
    leaf_1.ecx &=
        (Leaf1Ecx::ALL_FLAGS & !(Leaf1Ecx::OSXSAVE | Leaf1Ecx::MONITOR)).bits();
    leaf_1.edx &= Leaf1Edx::ALL_FLAGS.bits();
    let leaf_7 = host_cpuid.get_mut(&CpuidIdent::leaf(7)).unwrap();
    leaf_7.eax = 0;
    leaf_7.ebx &= (Leaf7Sub0Ebx::ALL_FLAGS
        & !(Leaf7Sub0Ebx::PQM
            | Leaf7Sub0Ebx::PQE
            | Leaf7Sub0Ebx::RDSEED
            | Leaf7Sub0Ebx::INVPCID))
        .bits();
    leaf_7.ecx = 0;
    leaf_7.edx = 0;

    // Report that leaf 0x8000_0004 is the last extended leaf and clean up
    // feature bits in leaf 0x8000_0001.
    host_cpuid.get_mut(&CpuidIdent::leaf(0x8000_0000)).unwrap().eax =
        0x8000_0004;
    let ext_leaf_1 =
        host_cpuid.get_mut(&CpuidIdent::leaf(0x8000_0001)).unwrap();
    ext_leaf_1.ecx &= (AmdExtLeaf1Ecx::ALT_MOV_CR8
        | AmdExtLeaf1Ecx::ABM
        | AmdExtLeaf1Ecx::SSE4A
        | AmdExtLeaf1Ecx::MISALIGN_SSE
        | AmdExtLeaf1Ecx::THREED_NOW_PREFETCH
        | AmdExtLeaf1Ecx::DATA_ACCESS_BP
        | AmdExtLeaf1Ecx::DATA_BP_ADDR_MASK_EXT)
        .bits();
    ext_leaf_1.edx &=
        (AmdExtLeaf1Edx::ALL_FLAGS & !AmdExtLeaf1Edx::RDTSCP).bits();

    // Test the plumbing by pumping a fake processor brand string into extended
    // leaves 2-4 and seeing if the guest recognizes it.
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

    host_cpuid.insert(CpuidIdent::leaf(0x8000_0002), ext_leaf_2);
    host_cpuid.insert(CpuidIdent::leaf(0x8000_0003), ext_leaf_3);
    host_cpuid.insert(CpuidIdent::leaf(0x8000_0004), ext_leaf_4);

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
}
