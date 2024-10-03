// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use cpuid_utils::{CpuidLeaf, CpuidVendor};
use phd_testcase::*;
use propolis_client::types::{Cpuid, CpuidEntry};
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
    let cpuid = match spec.board.cpuid {
        Cpuid::HostDefault => panic!("shouldn't have host default CPUID"),
        Cpuid::Template { entries, .. } => entries,
    };

    assert_eq!(cpuid.len(), entries.len());
    itertools::assert_equal(cpuid, entries);
}

#[phd_testcase]
async fn cpuid_boot_test(ctx: &Framework) {
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
        .get(&CpuidLeaf::leaf(0))
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
    host_cpuid.retain(|leaf, _values| {
        leaf.leaf <= 0x7
            || (leaf.leaf >= 0x8000_0000 && leaf.leaf <= 0x8000_0001)
    });

    // Mask off feature bits that the Oxide platform won't support. See RFD
    // 314. These are masks (and not assignments) so that features the host CPU
    // support won't appear in the guest's feature masks.
    host_cpuid.get_mut(&CpuidLeaf::leaf(0)).unwrap().eax = 7;
    let leaf_1 = host_cpuid.get_mut(&CpuidLeaf::leaf(1)).unwrap();
    leaf_1.ecx &= 0xF6F8_3203;
    leaf_1.edx &= 0x178B_FBFF;
    let leaf_7 = host_cpuid.get_mut(&CpuidLeaf::leaf(7)).unwrap();
    leaf_7.eax = 0;
    leaf_7.ebx &= 0x219C_03A9;
    leaf_7.ecx = 0;
    leaf_7.edx = 0;

    host_cpuid.get_mut(&CpuidLeaf::leaf(0x8000_0000)).unwrap().eax =
        0x8000_0001;
    let ext_leaf_1 = host_cpuid.get_mut(&CpuidLeaf::leaf(0x8000_0001)).unwrap();
    ext_leaf_1.ecx &= 0x4440_01F0;
    ext_leaf_1.edx &= 0x27D3_FBFF;

    // Try to boot a guest with the computed CPUID values.
    //
    // Ideally, this test would also verify that the guest actually perceives
    // the CPUID values that were passed to it, but the correct way to do that
    // varies heavily from guest OS to guest OS.
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
}
