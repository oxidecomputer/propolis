// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
use phd_framework::{test_vm::MigrationTimeout, TestVm};
use phd_testcase::*;
use propolis_client::{
    instance_spec::{CpuidEntry, VersionedInstanceSpec},
    types::InstanceSpecStatus,
};
use itertools::Itertools;
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

/// A synthetic brand string that can be injected into guest CPUID leaves
/// 0x8000_0002-0x8000_0004.
const BRAND_STRING: &[u8; 48] =
    b"Oxide Cloud Computer Company Cloud Computer\0\0\0\0\0";

/// Injects a fake CPU brand string into CPUID leaves 0x8000_0002-0x8000_0004.
///
/// # Panics
///
/// Panics if the input CPUID set does not include the brand string leaves.
fn inject_brand_string(cpuid: &mut CpuidSet) {
    // The brand string leaves have been defined for long enough that they
    // should be present on virtually any host that's modern enough to run
    // Propolis and PHD. Assert (instead of returning a "skipped" result) if
    // they're missing, since that may indicate a latent bug in the
    // `cpuid_utils` crate.
    let ext_leaf_0 = cpuid
        .get(CpuidIdent::leaf(cpuid_utils::bits::EXTENDED_BASE_LEAF))
        .expect("PHD-capable processors should have some extended leaves");

    assert!(
        ext_leaf_0.eax >= 0x8000_0004,
        "PHD-capable processors should support at least leaf 0x8000_0004 \
        (reported {})",
        ext_leaf_0.eax
    );

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

    cpuid.insert(CpuidIdent::leaf(0x8000_0002), ext_leaf_2).unwrap();
    cpuid.insert(CpuidIdent::leaf(0x8000_0003), ext_leaf_3).unwrap();
    cpuid.insert(CpuidIdent::leaf(0x8000_0004), ext_leaf_4).unwrap();
}

/// Asserts that `/proc/cpuinfo` in the guest returns output that contains
/// [`BRAND_STRING`].
async fn verify_guest_brand_string(vm: &TestVm) -> anyhow::Result<()> {
    let cpuinfo = vm.run_shell_command("cat /proc/cpuinfo").await?;
    info!(cpuinfo, "/proc/cpuinfo output");
    assert!(cpuinfo.contains(
        std::str::from_utf8(BRAND_STRING).unwrap().trim_matches('\0')
    ));

    Ok(())
}

/// Launches a test VM with a synthetic brand string injected into its CPUID
/// leaves.
async fn launch_cpuid_smoke_test_vm(
    ctx: &Framework,
    vm_name: &str,
) -> anyhow::Result<TestVm> {
    let mut host_cpuid = cpuid_utils::host::query_complete(
        cpuid_utils::host::CpuidSource::BhyveDefault,
    )?;

    info!(?host_cpuid, "read bhyve default CPUID");

    inject_brand_string(&mut host_cpuid);

    let mut cfg = ctx.vm_config_builder(vm_name);
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

    Ok(vm)
}

#[phd_testcase]
async fn cpuid_boot_test(ctx: &Framework) {
    let vm = launch_cpuid_smoke_test_vm(ctx, "cpuid_boot_test").await?;
    verify_guest_brand_string(&vm).await?;
}

#[phd_testcase]
async fn cpuid_migrate_smoke_test(ctx: &Framework) {
    let vm = launch_cpuid_smoke_test_vm(ctx, "cpuid_boot_test").await?;
    verify_guest_brand_string(&vm).await?;

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
    verify_guest_brand_string(&target).await?;
}

struct LinuxGuestTopo<'a> {
    vm: &'a TestVm
}

impl<'a> LinuxGuestTopo<'a> {
    fn cpu_stem(vcpu: u8) -> String {
        format!("/sys/devices/system/cpu/cpu{vcpu}/topology")
    }

    async fn new(vm: &'a TestVm) -> Self {
        let this = Self {
            vm
        };
        // Expect Linux numbers CPUs as 0 through vCPU-1 (inclusive).
        //
        // cpu0 should always exist (if it does not, /sys/devices/system is not
        // what we expect), and cpu<vCPU> should not (if it does, again,
        // /sys/devices/system is not what we expect).
        let out = this.vm.run_shell_command(
            &format!("ls {}", Self::cpu_stem(0))).await.expect("can run ls of a directory that exists");
        assert!(out.contains("thread_siblings"));

        let out = this.vm.run_shell_command(
            &format!("ls {}", Self::cpu_stem(this.cpus().await))).await.expect("can run ls of a directory that doesn't exist");
        assert!(out.contains("No such file or directory"));

        this
    }

    async fn cpus(&self) -> u8 {
        let spec_get_response = self.vm.get_spec().await.expect("can get the instance's spec back");
        let InstanceSpecStatus::Present(VersionedInstanceSpec::V0(spec)) =
            spec_get_response.spec
        else {
            panic!("instance spec should be present for a running VM");
        };

        spec.board.cpus
    }

    async fn physical_package_ids(&self) -> impl Iterator<Item=u32> {
        let mut result = Vec::new();
        for cpu_num in 0..self.cpus().await {
            let out = self.vm.run_shell_command(
                &format!("cat {}/physical_package_id", Self::cpu_stem(cpu_num))
            ).await.expect("can get cores' physical package ID");
            // Linux' `Documentation/API/stable/sysfs-devices-system-cpu` says
            // this is "integer" but drivers/base/topology.c says it is "%d"
            // specifically.
            result.push(u32::from_str_radix(&out, 10).expect("physical package id parses"));
        }
        result.into_iter()
    }

    /// Returns an iterator of hexadecimal bitmaps from Linux's determined
    /// thread sharing in the guest VM.
    ///
    /// Since Linux uses one bit per CPU to show if threads are shared in a
    /// processor core, this would use 65 bits for a 65-vCPU guest, or 256 bits
    /// for a 256-vCPU guest. So use `String` here instead of parsing into a
    /// numeric type so at least this "just works" when we get to larger core
    /// counts.
    async fn thread_siblings(&self) -> impl Iterator<Item=String> {
        let mut result = Vec::new();
        for cpu_num in 0..self.cpus().await {
            let out = self.vm.run_shell_command(
                &format!("cat {}/thread_siblings", Self::cpu_stem(cpu_num))
            ).await.expect("can get thread siblings of a core that exists");
            let out = out.trim();
            // Linux' `Documentation/API/stable/sysfs-devices-system-cpu` says
            // this is "hexadecimal bitmask." This is kept a string for reasons
            // described above so just validate the characters in it are
            // reasonable.
            assert!(out.chars().all(|c| c.is_ascii_hexdigit()));
            result.push(out.to_string());
        }
        result.into_iter()
    }
}

#[phd_testcase]
async fn guest_cpu_topo_test(ctx: &Framework) {
    let vm = launch_cpuid_smoke_test_vm(ctx, "guest_cpu_topo_test").await?;

    // Between the way we set initial APIC IDs and the way Linux numbers logical
    // processors, a 4-vCPU VM should report a topology like:
    // * core 0: thread 0, thread 1
    // * core 1: thread 2, thread 3
    //
    // over in sysfs, the CPU topology files then represent sibling-ness as a
    // bitfield. So core 0 has two threads (0, 1) and their bits are set in core
    // 0's thread_slibings (0b0000_0011 -> "3").  Core 1 does the same for
    // threads 2 and 3, which yields an expected core 1 thread_siblings of
    // (0b0000_1100 -> "c").
    let guest_topo = LinuxGuestTopo::new(&vm).await;

    // All cores should be in socket 0
    assert!(guest_topo.physical_package_ids().await.all(|item| item == 0));

    // We currently number CPUs such that Linux numbers them as successive pairs
    // of thread twins.
    let siblings = guest_topo.thread_siblings().await;
    for (idx, mut pair) in siblings.chunks(2).into_iter().enumerate() {
        let lower = pair.next().expect("sibling pair has a pair of cores");
        let upper = pair.next().expect("pairs have even numbers of cores");

        // Each pair of siblings should see that they have the same siblings
        assert_eq!(lower, upper);
        // This character in the string should have a pair of bits for the
        // current sibling pair under consideration.
        let sibling_idx = idx / 4;
        // And at that index, the character should be this hex digit
        // (representing the pair of bits for these sibling threads). We can be
        // looking either of the lower pairs (in which case the cores are 0b0011
        // => 3), or the higher pairs (in which case the cores are 0b1100 => c)
        let sibling_char = match idx % 4 {
            0 => '3',
            1 => '3',
            2 => 'c',
            3 => 'c',
            o => {
                panic!("bit index in hex digit is less than four? except {o}");
            }
        };
        assert!(lower.chars().enumerate().all(|(i, ch)| {
            if i != sibling_idx {
                ch == '0'
            } else {
                ch == sibling_char
            }
        }));
    }
}
