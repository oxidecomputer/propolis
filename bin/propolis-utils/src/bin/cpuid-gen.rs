// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cmp::{Ord, Ordering};
use std::collections::BTreeMap;

use bhyve_api::{VmmCtlFd, VmmFd};
use clap::Parser;

fn create_vm() -> anyhow::Result<VmmFd> {
    let name = format!("cpuid-gen-{}", std::process::id());
    let mut req =
        bhyve_api::vm_create_req::new(name.as_bytes()).expect("valid VM name");

    let ctl = VmmCtlFd::open()?;
    let _ = unsafe { ctl.ioctl(bhyve_api::VMM_CREATE_VM, &mut req) }?;

    let vm = VmmFd::open(&name).map_err(|e| {
        // Attempt to manually destroy the VM if we cannot open it
        let _ = ctl.vm_destroy(name.as_bytes());
        e
    })?;

    vm.ioctl_usize(bhyve_api::ioctls::VM_SET_AUTODESTRUCT, 1).map_err(|e| {
        // Destroy instance if auto-destruct cannot be set
        let _ = vm.ioctl_usize(bhyve_api::VM_DESTROY_SELF, 0);
        e
    })?;

    Ok(vm)
}

#[derive(Clone, Copy, Default, Debug)]
struct Cpuid {
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}
impl Cpuid {
    const fn is_authentic_amd(&self) -> bool {
        self.ebx == 0x68747541
            && self.ecx == 0x444d4163
            && self.edx == 0x69746e65
    }
    const fn all_zeros(&self) -> bool {
        self.eax == 0 && self.ebx == 0 && self.ecx == 0 && self.edx == 0
    }
}
impl From<&bhyve_api::vm_legacy_cpuid> for Cpuid {
    fn from(value: &bhyve_api::vm_legacy_cpuid) -> Self {
        Self {
            eax: value.vlc_eax,
            ebx: value.vlc_ebx,
            ecx: value.vlc_ecx,
            edx: value.vlc_edx,
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum CpuidKey {
    Leaf(u32),
    SubLeaf(u32, u32),
    LeafDefault(u32),
}
impl CpuidKey {
    const fn eax(&self) -> u32 {
        match self {
            CpuidKey::Leaf(eax) => *eax,
            CpuidKey::SubLeaf(eax, _) => *eax,
            CpuidKey::LeafDefault(eax) => *eax,
        }
    }
}
impl Ord for CpuidKey {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.eq(other) {
            return Ordering::Equal;
        }
        match self.eax().cmp(&other.eax()) {
            Ordering::Equal => match (self, other) {
                (CpuidKey::Leaf(_), _) | (_, CpuidKey::LeafDefault(_)) => {
                    Ordering::Greater
                }
                (CpuidKey::LeafDefault(_), _) | (_, CpuidKey::Leaf(_)) => {
                    Ordering::Less
                }
                (CpuidKey::SubLeaf(_, ecx), CpuidKey::SubLeaf(_, oecx)) => {
                    ecx.cmp(oecx)
                }
            },
            o => o,
        }
    }
}
impl PartialOrd for CpuidKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Query CPUID through bhyve-defined masks
fn query_cpuid(vm: &VmmFd, eax: u32, ecx: u32) -> anyhow::Result<Cpuid> {
    let mut data = bhyve_api::vm_legacy_cpuid {
        vlc_eax: eax,
        vlc_ecx: ecx,
        ..Default::default()
    };
    unsafe { vm.ioctl(bhyve_api::VM_LEGACY_CPUID, &mut data) }?;
    Ok(Cpuid::from(&data))
}

/// Query CPUID directly from host CPU
fn query_raw_cpuid(eax: u32, ecx: u32) -> Cpuid {
    let mut res = Cpuid::default();

    unsafe {
        std::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) res.ebx,
            // select cpuid 0, also specify eax as clobbered
            inout("eax") eax => res.eax,
            inout("ecx") ecx => res.ecx,
            out("edx") res.edx,
        );
    }
    res
}

const STD_EAX_BASE: u32 = 0x0;
const EXTD_EAX_BASE: u32 = 0x80000000;

const CPU_FEAT_ECX_XSAVE: u32 = 1 << 26;

fn collect_cpuid(
    query_cpuid: &impl Fn(u32, u32) -> anyhow::Result<Cpuid>,
    zero_elide: bool,
) -> anyhow::Result<BTreeMap<CpuidKey, Cpuid>> {
    let std = query_cpuid(STD_EAX_BASE, 0)?;
    let extd = query_cpuid(EXTD_EAX_BASE, 0)?;

    let mut results: BTreeMap<CpuidKey, Cpuid> = BTreeMap::new();

    let mut xsave_supported = false;
    let is_amd = std.is_authentic_amd();

    for eax in 0..=std.eax {
        let data = query_cpuid(eax, 0)?;

        if zero_elide && data.all_zeros() {
            continue;
        }

        match eax {
            0x1 => {
                if data.ecx & CPU_FEAT_ECX_XSAVE != 0 {
                    xsave_supported = true;
                }
                results.insert(CpuidKey::Leaf(eax), data);
            }
            0x7 => {
                results.insert(CpuidKey::SubLeaf(eax, 0), data);

                // TODO: handle more sub-leaf entries?

                // Default entry for invalid sub-leaf is all-zeroes
                results.insert(CpuidKey::LeafDefault(eax), Cpuid::default());
            }
            0xb if is_amd => {
                // Extended topo

                // TODO: AMD_specifc handling
                results.insert(CpuidKey::Leaf(eax), data);
            }
            0xd if xsave_supported => {
                // XSAVE
                let xcr0_bits = data.eax as u64 | data.edx as u64;
                results.insert(CpuidKey::SubLeaf(eax, 0), data);
                let data = query_cpuid(eax, 1)?;
                let xss_bits = data.ecx as u64 | data.edx as u64;
                results.insert(CpuidKey::SubLeaf(eax, 1), data);

                // Fetch all the 2:63 sub-leaf entries which are valid
                for ecx in 2..63 {
                    if (1 << ecx) & (xcr0_bits | xss_bits) == 0 {
                        continue;
                    }
                    let data = query_cpuid(eax, ecx)?;
                    results.insert(CpuidKey::SubLeaf(eax, ecx), data);
                }
                // Default entry for invalid sub-leaf is all-zeroes
                results.insert(CpuidKey::LeafDefault(eax), Cpuid::default());
            }
            _ => {
                results.insert(CpuidKey::Leaf(eax), data);
            }
        }
    }

    for eax in EXTD_EAX_BASE..extd.eax {
        let data = query_cpuid(eax, 0)?;

        if zero_elide && data.all_zeros() {
            continue;
        }
        results.insert(CpuidKey::Leaf(eax), data);
    }

    Ok(results)
}

fn print_text(results: &BTreeMap<CpuidKey, Cpuid>) {
    for (key, value) in results.iter() {
        let header = match key {
            CpuidKey::Leaf(eax) | CpuidKey::LeafDefault(eax) => {
                format!("eax:{:x}\t\t", eax)
            }
            CpuidKey::SubLeaf(eax, ecx) => {
                format!("eax:{:x} ecx:{:x}", eax, ecx)
            }
        };

        println!(
            "{} ->\t{:x} {:x} {:x} {:x}",
            header, value.eax, value.ebx, value.ecx, value.edx
        );
    }
}
fn print_toml(results: &BTreeMap<CpuidKey, Cpuid>) {
    println!("[cpuid]");
    for (key, value) in results.iter() {
        let key_name = match key {
            CpuidKey::Leaf(eax) => format!("{:x}", eax),
            CpuidKey::SubLeaf(eax, ecx) => format!("{:x}-{:x}", eax, ecx),
            CpuidKey::LeafDefault(eax) => format!("{:x}-", eax),
        };
        println!(
            "\"{}\" = [0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}]",
            key_name, value.eax, value.ebx, value.ecx, value.edx
        );
    }
}

#[derive(clap::Parser)]
struct Opts {
    /// Elide all-zero entries from results
    #[clap(short)]
    zero_elide: bool,

    /// Emit toml instead of text
    #[clap(short)]
    toml_output: bool,

    /// Query CPU directly, rather that via bhyve masking
    #[clap(short)]
    raw_query: bool,
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    let queryf: Box<dyn Fn(u32, u32) -> anyhow::Result<Cpuid>> =
        if opts.raw_query {
            Box::new(|eax, ecx| Ok(query_raw_cpuid(eax, ecx)))
        } else {
            let vm = create_vm()?;
            Box::new(move |eax, ecx| query_cpuid(&vm, eax, ecx))
        };

    let results = collect_cpuid(&queryf, opts.zero_elide)?;

    if opts.toml_output {
        print_toml(&results);
    } else {
        print_text(&results);
    }

    Ok(())
}
