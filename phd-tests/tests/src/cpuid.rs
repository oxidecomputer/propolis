// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use cpuid_utils::{CpuidIdent, CpuidSet, CpuidValues};
use phd_framework::{test_vm::MigrationTimeout, TestVm};
use phd_testcase::*;
use propolis_client::instance_spec::{
    CpuTopology, CpuidEntry, InstanceSpecStatus,
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
async fn cpuid_instance_spec_round_trip_test(ctx: &TestCtx) {
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
    let InstanceSpecStatus::Present(spec) = spec_get_response.spec else {
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
///
/// Some tests cover specific CPU topologies, for which this helper is
/// unsuitable; instead, use `launch_cpuid_smoke_test_vm_with_topo` and use
/// explicit settings for the code under test.
async fn launch_cpuid_smoke_test_vm(
    ctx: &TestCtx,
    vm_name: &str,
) -> anyhow::Result<TestVm> {
    launch_cpuid_smoke_test_vm_with_topo(ctx, vm_name, None).await
}

async fn launch_cpuid_smoke_test_vm_with_topo(
    ctx: &TestCtx,
    vm_name: &str,
    guest_topo: Option<CpuTopology>,
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
    if let Some(topo) = guest_topo {
        cfg.cpu_topology(topo);
    }
    let mut vm = ctx.spawn_vm(&cfg, None).await?;
    vm.launch().await?;
    vm.wait_to_boot().await?;

    Ok(vm)
}

#[phd_testcase]
async fn cpuid_boot_test(ctx: &TestCtx) {
    let vm = launch_cpuid_smoke_test_vm(ctx, "cpuid_boot_test").await?;
    verify_guest_brand_string(&vm).await?;
}

#[phd_testcase]
async fn cpuid_migrate_smoke_test(ctx: &TestCtx) {
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
    vm: &'a TestVm,
}

struct CacheInfo {
    level: u8,
    shared_cpu_list: String,
}

impl<'a> LinuxGuestTopo<'a> {
    fn cpu_stem(vcpu: u16) -> String {
        format!("/sys/devices/system/cpu/cpu{vcpu}/topology")
    }

    fn cache_stem(vcpu: u16, index: u8) -> String {
        format!("/sys/devices/system/cpu/cpu{vcpu}/cache/index{index}")
    }

    async fn new(vm: &'a TestVm) -> Self {
        let this = Self { vm };
        // Expect Linux numbers CPUs as 0 through vCPU-1 (inclusive).
        //
        // cpu0 should always exist (if it does not, /sys/devices/system is not
        // what we expect), and cpu<vCPU> should not (if it does, again,
        // /sys/devices/system is not what we expect).
        let out = this
            .vm
            .run_shell_command(&format!("ls {}", Self::cpu_stem(0)))
            .await
            .expect("can run ls of a directory that exists");
        assert!(out.contains("thread_siblings"));

        let out = this
            .vm
            .run_shell_command(&format!(
                "ls {}",
                Self::cpu_stem(this.cpus().await)
            ))
            .ignore_status()
            .await
            .expect("can run ls of a directory that doesn't exist");
        assert!(out.contains("No such file or directory"));

        this
    }

    async fn cpus(&self) -> u16 {
        let spec_get_response =
            self.vm.get_spec().await.expect("can get the instance's spec back");
        let InstanceSpecStatus::Present(spec) = spec_get_response.spec else {
            panic!("instance spec should be present for a running VM");
        };

        spec.board.cpus
    }

    async fn physical_package_ids(&self) -> impl Iterator<Item = u32> {
        let mut result = Vec::new();
        for cpu_num in 0..self.cpus().await {
            let out = self
                .vm
                .run_shell_command(&format!(
                    "cat {}/physical_package_id",
                    Self::cpu_stem(cpu_num)
                ))
                .await
                .expect("can get cores' physical package ID");
            // Linux' `Documentation/API/stable/sysfs-devices-system-cpu` says
            // this is "integer" but drivers/base/topology.c says it is "%d"
            // specifically.
            result
                .push(out.parse::<u32>().expect("physical package id parses"));
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
    async fn thread_siblings(&self) -> impl Iterator<Item = String> {
        let mut result = Vec::new();
        for cpu_num in 0..self.cpus().await {
            let out = self
                .vm
                .run_shell_command(&format!(
                    "cat {}/thread_siblings",
                    Self::cpu_stem(cpu_num)
                ))
                .await
                .expect("can get thread siblings of a core that exists");
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

    /// Returns true if the guest has SMT enabled.
    ///
    /// This is determined by checking if cpu0's thread_siblings has more than
    /// one bit set.
    async fn has_smt(&self) -> bool {
        let siblings = self
            .vm
            .run_shell_command(&format!(
                "cat {}/thread_siblings",
                Self::cpu_stem(0)
            ))
            .await
            .expect("can get thread siblings");
        let siblings = siblings.trim();

        let value = u64::from_str_radix(siblings, 16)
            .expect("thread_siblings should be valid hex");
        value.count_ones() > 1
    }

    async fn cache_info(&self) -> Vec<CacheInfo> {
        let mut result = Vec::new();
        for index in 0u8.. {
            let stem = Self::cache_stem(0, index);
            let level_out = self
                .vm
                .run_shell_command(&format!("cat {stem}/level 2>/dev/null"))
                .ignore_status()
                .await
                .expect("can run cat");

            let level_out = level_out.trim();
            if level_out.is_empty() || level_out.contains("file not found") {
                break;
            }

            let level: u8 = level_out.parse().expect("cache level parses");

            let shared_cpu_list = self
                .vm
                .run_shell_command(&format!("cat {stem}/shared_cpu_list"))
                .await
                .expect("can read shared_cpu_list")
                .trim()
                .to_string();

            result.push(CacheInfo { level, shared_cpu_list });
        }
        result
    }
}

async fn check_legacy_topo(guest_topo: &LinuxGuestTopo<'_>) {
    // Because the "legacy" (AMD) topology is single-thread sockets, a bunch of
    // things that are seemingly bogus.

    // All cores should be in a distinct socket matching their APIC ID.
    for (idx, item) in guest_topo.physical_package_ids().await.enumerate() {
        let item: usize = item.try_into().unwrap();
        assert_eq!(idx, item);
    }

    // Single-thread sockets, so no SMT.
    assert!(!guest_topo.has_smt().await);

    // thread_siblings() should only have a single bit, matching the core's ID.
    let siblings: Vec<_> = guest_topo.thread_siblings().await.collect();
    for (idx, siblings) in siblings.iter().enumerate() {
        let core_char = idx / 2;
        let mask = 1 << (idx % 4);
        // A truly egregious conversion from single digit integer to `char`, but
        // I don't want you to have to read `(0x30 + mask) as char`
        let mask_str = mask.to_string();
        assert_eq!(mask_str.len(), 1);
        let expected_char = mask_str.chars().next().unwrap();
        assert!(siblings.chars().enumerate().all(|(i, ch)| {
            if i != core_char {
                ch == '0'
            } else {
                ch == expected_char
            }
        }));
    }
}

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
async fn check_smt_topo(guest_topo: &LinuxGuestTopo<'_>) {
    // All cores should be in socket 0
    assert!(guest_topo.physical_package_ids().await.all(|item| item == 0));

    assert!(guest_topo.has_smt().await);

    // We currently number CPUs such that Linux numbers them as successive
    // pairs of thread twins.
    let siblings: Vec<_> = guest_topo.thread_siblings().await.collect();
    for (idx, pair) in siblings.chunks(2).enumerate() {
        assert!(pair.len() == 2, "expected even number of CPUs");
        let lower = &pair[0];
        let upper = &pair[1];

        // Each pair of siblings should see that they have the same siblings
        assert_eq!(lower, upper);
        // Each hex digit represents 4 CPUs. With chunks(2), idx is the pair
        // number: idx=0 is cpus 0-1, idx=1 is cpus 2-3, etc.
        let sibling_idx = idx / 2;
        let sibling_char = if idx % 2 == 0 { '3' } else { 'c' };
        assert!(lower.chars().enumerate().all(|(i, ch)| {
            if i != sibling_idx {
                ch == '0'
            } else {
                ch == sibling_char
            }
        }));
    }
}

// Check cache topology from the perspective of core 0.
//
// We should see L1i, L1d, L2, and L3 caches, where L1 and L2 caches are shared
// iff SMT is enabled. L3's sharing depends on the CPU topology, so what we
// should expect is really only known by the caller.
async fn check_cache_topo(
    guest_topo: &LinuxGuestTopo<'_>,
    expected_l1_caches: usize,
    l3_sharing: &str,
) {
    let caches = guest_topo.cache_info().await;
    let has_smt = guest_topo.has_smt().await;

    for cache in &caches {
        match cache.level {
            1 | 2 => {
                let expected = if has_smt { "0-1" } else { "0" };
                assert_eq!(cache.shared_cpu_list, expected);
            }
            3 => {
                assert_eq!(cache.shared_cpu_list, l3_sharing);
            }
            other => {
                panic!("unexpected cache level {other}");
            }
        }
    }

    let l1_count = caches.iter().filter(|c| c.level == 1).count();
    let l2_count = caches.iter().filter(|c| c.level == 2).count();

    if l1_count != expected_l1_caches {
        match expected_l1_caches {
            1 => {
                panic!("expected one L1 cache, found {}", l1_count);
            }
            2 => {
                panic!("expected L1d and L1i caches, found {}", l1_count);
            }
            other => {
                panic!("expected {} L1 caches, found {}", other, l1_count);
            }
        };
    }
    assert!(l2_count >= 1, "expected at least one L2 cache");
}

/// This intentionally-reductive list of CPU vendors is only because Propolis is
/// primarily tested on AMD x86-64 processors, is expected to reasonably work on
/// Intel x86-64 processors, and expects no particular suppport elsewhere.
/// (yet?)
enum CpuVendor {
    Intel,
    Amd,
}

fn host_cpu_vendor() -> CpuVendor {
    let mut vendor_bytes = [0u8; 12];
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            let result = core::arch::x86_64::__cpuid_count(0, 0);
            vendor_bytes[0..4].copy_from_slice(&result.ebx.to_le_bytes());
            vendor_bytes[4..8].copy_from_slice(&result.edx.to_le_bytes());
            vendor_bytes[8..12].copy_from_slice(&result.ecx.to_le_bytes());
        } else {
            // You're running PHD on something other than x86-64?
            // Have you ported Propolis to ARM? Cool!
            panic!("host CPUID queries only work on x86-64 hosts");
        }
    }
    match &vendor_bytes {
        b"GenuineIntel" => CpuVendor::Intel,
        b"AuthenticAMD" => CpuVendor::Amd,
        other => {
            let brand_string = String::from_utf8_lossy(other.as_slice());
            panic!("unknown CPUID brand string: {}", brand_string);
        }
    }
}

#[phd_testcase]
async fn guest_cpu_legacy_topo_test(ctx: &TestCtx) {
    // As of the introduction of explicit CPU topology configuration, the extant
    // Intel behavior was to provide a single socket, while the extant AMD
    // behavior was, accidentally, to provide a single-thread-per-socket
    // topology.
    //
    // Now, the test harness picks a single-socket default, and
    // topology-sensitive tests pick expected VM topologies specifically. This
    // test is "the prior Propolis topology is unchanged". The only reason we'd
    // care to preserve the single-thread-per-socket half-specialized topology
    // is for use in the product's "initial" virtual platform with several other
    // decisions which would break guests to change.
    let legacy_topo = match host_cpu_vendor() {
        CpuVendor::Intel => CpuTopology::single_socket_smt(),
        CpuVendor::Amd => CpuTopology::legacy_single_thread_sockets(2),
    };

    let vm = launch_cpuid_smoke_test_vm_with_topo(
        ctx,
        "guest_cpu_legacy_topo_test",
        Some(legacy_topo),
    )
    .await?;

    // The topology-checking is Linux-specific, though it should be appropriate
    // for all Linux distributions. For other OSes, just warn and skip for now.
    if !vm.guest_os_kind().is_linux() {
        phd_skip!(format!(
            "guest topo test does not yet have support for {:?}",
            vm.guest_os_kind()
        ));
    }

    let guest_topo = LinuxGuestTopo::new(&vm).await;

    let expected_l3_share = match host_cpu_vendor() {
        CpuVendor::Intel => {
            let num_cpus = guest_topo.cpus().await;
            format!("0-{}", num_cpus - 1)
        }
        CpuVendor::Amd => "0".to_string(),
    };

    match host_cpu_vendor() {
        CpuVendor::Intel => check_smt_topo(&guest_topo).await,
        CpuVendor::Amd => check_legacy_topo(&guest_topo).await,
    };

    check_cache_topo(&guest_topo, 2, &expected_l3_share).await;
}

#[phd_testcase]
async fn guest_cpu_single_socket_smt(ctx: &TestCtx) {
    // Now and in the future we can explicitly request a CPU topology via
    // propolis-server's API, and control which leaves reflect that topology. In
    // all cases if we set a single-socket topology, that's what the guest
    // should show.
    let test_topo = CpuTopology::single_socket_smt();

    let vm = launch_cpuid_smoke_test_vm_with_topo(
        ctx,
        "guest_cpu_single_socket_smt",
        Some(test_topo),
    )
    .await?;

    // The topology-checking is Linux-specific, though it should be appropriate
    // for all Linux distributions. For other OSes, just warn and skip for now.
    if !vm.guest_os_kind().is_linux() {
        phd_skip!(format!(
            "guest topo test does not yet have support for {:?}",
            vm.guest_os_kind()
        ));
    }

    let guest_topo = LinuxGuestTopo::new(&vm).await;

    let num_cpus = guest_topo.cpus().await;
    let expected_l3_share = format!("0-{}", num_cpus - 1);

    let expected_l1_caches = match host_cpu_vendor() {
        CpuVendor::Intel => 2,
        CpuVendor::Amd => {
            // TODO: PHD does not yet get the full CPUID profile leaves that
            // Omicron would send a real Propolis in the product, which means
            // that the CPU topology we test against here is Propolis'
            // specialization of bhyve defaults. Since we enable
            // TopologyExtensions the guest may reasonably consider leaf
            // 8000_001D, but legacy_emulate_cpuid will present a three-cache
            // layout with an L1, an L2, and an L3.
            //
            // In the product we present a Zen-like L1i, L1d, L2, and L3, so we
            // *should* expect two L1 caches here in a product-like VM. When we
            // extract virtual platform details from Omicron and can use them
            // here, PHD VMs will have two L1s as they should and we'll update
            // this to expect two L1 caches in either vendor's case.
            //
            // XXX: It's pretty weird we'd look for two L1 caches above, but now
            // with the default bhyve 8000_001D setup we're back to one, right?
            // The two-L1 notion comes from leaf 8000_0005, which it turns out
            // bhyve does not mask, so there are non-zero-size L1i and L1d. When
            // 8000_001D is available it takes precedence for the layout, so
            // bhyve's default topology applies instead. This is probably a bug
            // to fix in bhyve's legacy_emulate_cpuid to make these consistent.
            1
        }
    };

    check_smt_topo(&guest_topo).await;

    check_cache_topo(&guest_topo, expected_l1_caches, &expected_l3_share).await;
}
