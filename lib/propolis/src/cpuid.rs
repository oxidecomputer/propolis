// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(unused)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU8;
use std::ops::Bound;

use bhyve_api::vcpu_cpuid_entry;
use cpuid_utils::CpuidMap;
use propolis_types::{CpuidLeaf, CpuidValues, CpuidVendor};

/// Set of cpuid leafs
#[derive(Clone, Debug)]
pub struct Set {
    map: CpuidMap,
    pub vendor: CpuidVendor,
}

impl Default for Set {
    fn default() -> Self {
        Self::new_host()
    }
}

impl Set {
    pub fn new(vendor: CpuidVendor) -> Self {
        Set { map: CpuidMap::default(), vendor }
    }

    pub fn new_host() -> Self {
        let vendor =
            CpuidVendor::try_from(cpuid_utils::host_query(CpuidLeaf::leaf(0)))
                .expect("host CPU should be from recognized vendor");
        Self::new(vendor)
    }

    pub fn new_from_map(map: CpuidMap, vendor: CpuidVendor) -> Self {
        Self { map, vendor }
    }

    pub fn insert(
        &mut self,
        ident: CpuidLeaf,
        entry: CpuidValues,
    ) -> Option<CpuidValues> {
        self.map.0.insert(ident, entry)
    }
    pub fn remove(&mut self, ident: CpuidLeaf) -> Option<CpuidValues> {
        self.map.0.remove(&ident)
    }
    pub fn remove_all(&mut self, func: u32) {
        self.map.0.retain(|ident, _val| ident.leaf != func);
    }
    pub fn get(&self, ident: CpuidLeaf) -> Option<&CpuidValues> {
        self.map.0.get(&ident)
    }
    pub fn get_mut(&mut self, ident: CpuidLeaf) -> Option<&mut CpuidValues> {
        self.map.0.get_mut(&ident)
    }
    pub fn is_empty(&self) -> bool {
        self.map.0.is_empty()
    }

    pub fn iter(&self) -> Iter {
        Iter(self.map.0.iter())
    }

    pub fn for_regs(&self, eax: u32, ecx: u32) -> Option<CpuidValues> {
        if let Some(ent) = self.map.0.get(&CpuidLeaf::subleaf(eax, ecx)) {
            // Exact match
            Some(*ent)
        } else if let Some(ent) = self.map.0.get(&CpuidLeaf::leaf(eax)) {
            // Function-only match
            Some(*ent)
        } else {
            None
        }
    }

    pub fn into_inner(self) -> (CpuidMap, CpuidVendor) {
        (self.map, self.vendor)
    }
}

impl From<Set> for Vec<bhyve_api::vcpu_cpuid_entry> {
    fn from(value: Set) -> Self {
        let mut out = Vec::with_capacity(value.map.0.len());
        out.extend(value.map.0.iter().map(|(ident, leaf)| {
            let vce_flags = match ident.subleaf.as_ref() {
                Some(_) => bhyve_api::VCE_FLAG_MATCH_INDEX,
                None => 0,
            };
            bhyve_api::vcpu_cpuid_entry {
                vce_function: ident.leaf,
                vce_index: ident.subleaf.unwrap_or(0),
                vce_flags,
                vce_eax: leaf.eax,
                vce_ebx: leaf.ebx,
                vce_ecx: leaf.ecx,
                vce_edx: leaf.edx,
                ..Default::default()
            }
        }));
        out
    }
}

/// Convert a [vcpu_cpuid_entry] into an ([CpuidLeaf],
/// [CpuidValues]) tuple, suitable for insertion into a [Set].
///
/// This would be implemented as a [From] trait if rust let us.
pub fn from_raw(
    value: bhyve_api::vcpu_cpuid_entry,
) -> (CpuidLeaf, CpuidValues) {
    let subleaf = if value.vce_flags & bhyve_api::VCE_FLAG_MATCH_INDEX != 0 {
        Some(value.vce_index)
    } else {
        None
    };

    (
        CpuidLeaf { leaf: value.vce_function, subleaf },
        CpuidValues {
            eax: value.vce_eax,
            ebx: value.vce_ebx,
            ecx: value.vce_ecx,
            edx: value.vce_edx,
        },
    )
}

pub struct Iter<'a>(
    std::collections::btree_map::Iter<'a, CpuidLeaf, CpuidValues>,
);
impl<'a> Iterator for Iter<'a> {
    type Item = (&'a CpuidLeaf, &'a CpuidValues);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpecializeError {
    #[error("unsupported cache level")]
    UnsupportedCacheLevel,
    #[error("missing vcpu count")]
    MissingVcpuCount,
}

/// Specialize a set of cpuid leafs for provided attributes.
///
/// This includes things such as a CPU topology (cores/threads/etc), a given
/// vCPU ID (APIC, core/thread ID, etc), or other info tidbits.
#[derive(Default)]
pub struct Specializer {
    has_smt: bool,
    num_vcpu: Option<NonZeroU8>,
    vcpuid: Option<i32>,
    vendor_kind: Option<CpuidVendor>,
    cpu_topo_populate: BTreeSet<TopoKind>,
    cpu_topo_clear: BTreeSet<TopoKind>,
    do_cache_topo: bool,
}
impl Specializer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Specify number of vCPUs in instance, and if SMT is enabled
    pub fn with_vcpu_count(self, count: NonZeroU8, has_smt: bool) -> Self {
        Self { num_vcpu: Some(count), has_smt, ..self }
    }

    /// Specify vCPU ID to specialize for
    pub fn with_vcpuid(self, vcpuid: i32) -> Self {
        assert!((vcpuid as usize) < bhyve_api::VM_MAXCPU);
        Self { vcpuid: Some(vcpuid), ..self }
    }

    /// Specify CPU vendor
    pub fn with_vendor(self, vendor: CpuidVendor) -> Self {
        Self { vendor_kind: Some(vendor), ..self }
    }

    /// Specify CPU topology types to render into the specialized [Set]
    ///
    /// Without basic information such as the number of vCPUs (set by
    /// [`Self::with_vcpu_count()`]), population of the requested topology
    /// information may be incomplete.
    pub fn with_cpu_topo(
        self,
        populate: impl Iterator<Item = TopoKind>,
    ) -> Self {
        let mut cpu_topo_populate = BTreeSet::new();

        for t in populate {
            cpu_topo_populate.insert(t);
        }

        Self { cpu_topo_populate, ..self }
    }

    /// Specify CPU topology types to clear from the specialized [Set]
    ///
    /// Some leafs in the provided set may not match expectations for the given
    /// CPU vendor.  Without populating it with generated data (via
    /// [`Self::with_cpu_topo()`]), those leafs can be cleared out.
    pub fn clear_cpu_topo(self, clear: impl Iterator<Item = TopoKind>) -> Self {
        let mut cpu_topo_clear = BTreeSet::new();
        for t in clear {
            cpu_topo_clear.insert(t);
        }

        Self { cpu_topo_clear, ..self }
    }

    /// Update cache topology information for specified vCPU count and SMT
    /// capabilities
    pub fn with_cache_topo(self) -> Self {
        Self { do_cache_topo: true, ..self }
    }

    /// Given the attributes and modifiers specified in this [Specializer],
    /// render an updated [Set] reflecting those data.
    pub fn execute(self, mut set: Set) -> Result<Set, SpecializeError> {
        // Use vendor override if provided, or else the existing one
        if let Some(vendor) = self.vendor_kind {
            set.vendor = vendor;
        }
        match set.vendor {
            CpuidVendor::Amd => {
                if self.do_cache_topo && self.num_vcpu.is_some() {
                    self.fix_amd_cache_topo(&mut set)?;
                }
            }
            _ => {}
        }

        // apply any requested topo info fixups
        self.fix_cpu_topo(&mut set)?;

        // APIC ID based on vcpuid
        if let Some(vcpuid) = self.vcpuid.as_ref() {
            if let Some(ent) = set.get_mut(CpuidLeaf::leaf(1)) {
                // bits 31:24 contain initial APIC ID
                ent.ebx &= !0xff000000;
                ent.ebx |= ((*vcpuid as u32) & 0xff) << 24;
            }
        }

        // logical CPU count (if SMT is enabled)
        if let Some(num_vcpu) = self.num_vcpu.as_ref() {
            if self.has_smt {
                if let Some(ent) = set.get_mut(CpuidLeaf::leaf(1)) {
                    ent.edx |= (0x1 << 28);
                    // bits 23:16 contain max IDs for logical CPUs in package
                    ent.ebx &= !0xff0000;
                    ent.ebx |= u32::from(num_vcpu.get()) << 16;
                }
            }
        }

        Ok(set)
    }

    fn fix_amd_cache_topo(&self, set: &mut Set) -> Result<(), SpecializeError> {
        assert!(self.do_cache_topo);
        let num = self.num_vcpu.unwrap().get();
        for ecx in 0..u32::MAX {
            match set.get_mut(CpuidLeaf::subleaf(0x8000001d, ecx)) {
                None => break,
                Some(vals) => {
                    // bits 7:5 hold the cache level
                    let visible_count = match (vals.eax & 0b11100000 >> 5) {
                        0b001 | 0b010 => {
                            // L1/L2 shared by SMT siblings
                            if self.has_smt {
                                2
                            } else {
                                1
                            }
                        }
                        0b011 => {
                            // L3 shared by all vCPUs
                            // TODO: segregate by sockets, if configured
                            u32::from(num)
                        }
                        _ => {
                            // unceremonious handling of unexpected cache levels
                            return Err(SpecializeError::UnsupportedCacheLevel);
                        }
                    };
                    // the number of logical CPUs (minus 1) sharing this cache
                    // is stored in bits 25:14
                    vals.eax &= !(0xfff << 14);
                    vals.eax |= (visible_count - 1) << 14;
                }
            }
        }
        Ok(())
    }
    fn fix_cpu_topo(&self, set: &mut Set) -> Result<(), SpecializeError> {
        for topo in self.cpu_topo_populate.union(&self.cpu_topo_clear) {
            // Nuke any existing info in order to potentially override it
            let leaf = *topo as u32;
            set.remove_all(leaf);

            if !self.cpu_topo_populate.contains(topo) {
                continue;
            }

            let num_vcpu = self
                .num_vcpu
                .ok_or(SpecializeError::MissingVcpuCount)
                .map(|n| u32::from(n.get()))?;

            match topo {
                TopoKind::StdB => {
                    // Queries with invalid ecx will get all-zeroes
                    set.insert(CpuidLeaf::leaf(leaf), CpuidValues::default());
                    if self.has_smt {
                        set.insert(
                            CpuidLeaf::subleaf(leaf, 0),
                            CpuidValues {
                                eax: 0x1,
                                ebx: 0x2,
                                ecx: 0x100,
                                // TODO: populate with x2APIC ID
                                edx: 0x0,
                            },
                        );
                    } else {
                        set.insert(
                            CpuidLeaf::subleaf(leaf, 0),
                            CpuidValues {
                                eax: 0x0,
                                ebx: 0x1,
                                ecx: 0x100,
                                // TODO: populate with x2APIC ID
                                edx: 0x0,
                            },
                        );
                    }
                    set.insert(
                        CpuidLeaf::subleaf(leaf, 1),
                        CpuidValues {
                            eax: 0x0,
                            ebx: num_vcpu,
                            ecx: 0x201,
                            // TODO: populate with x2APIC ID
                            edx: 0x0,
                        },
                    );
                }
                TopoKind::Std1F => {
                    // TODO: add 0x1f topo info
                }
                TopoKind::Ext1E => {
                    let id = self.vcpuid.unwrap_or(0) as u32;
                    let mut ebx = id;
                    if self.has_smt {
                        // bits 15:8 hold the zero-based threads-per-compute-unit
                        ebx |= 0x100;
                    }
                    set.insert(
                        CpuidLeaf::leaf(leaf),
                        CpuidValues {
                            eax: id,
                            ebx,
                            // TODO: populate ecx info?
                            ecx: 0,
                            edx: 0,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

/// Flavors of CPU topology information
#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq, strum::EnumIter)]
pub enum TopoKind {
    /// Leaf 0xB AMD (and legacy on Intel)
    StdB = 0xb,
    /// Leaf 0x1F (Intel)
    Std1F = 0x1f,
    /// LEAF 0x8000001E (AMD)
    Ext1E = 0x8000001e,
}

/// Parse the Processor Brand String (aka ProcName) from extended leafs
/// 0x8000_0002 - 0x8000_0004.
pub fn parse_brand_string(
    leafs: [CpuidValues; 3],
) -> Result<String, std::str::Utf8Error> {
    let mut buf = Vec::with_capacity(16 * 3);
    for ent in leafs {
        buf.extend_from_slice(&ent.eax.to_le_bytes());
        buf.extend_from_slice(&ent.ebx.to_le_bytes());
        buf.extend_from_slice(&ent.ecx.to_le_bytes());
        buf.extend_from_slice(&ent.edx.to_le_bytes());
    }
    // remove NUL and trailing chars
    if let Some(nul_pos) = buf.iter().position(|c| *c == 0) {
        buf.truncate(nul_pos);
    }
    let untrimmed = std::str::from_utf8(&buf)?;

    // trim any bounding whitespace which remains
    Ok(untrimmed.trim().to_string())
}
