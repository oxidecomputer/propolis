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
use cpuid_utils::{CpuidIdent, CpuidMap, CpuidSet, CpuidValues, CpuidVendor};

/// Convert a [vcpu_cpuid_entry] into an ([CpuidIdent],
/// [CpuidValues]) tuple, suitable for insertion into a [CpuidSet].
///
/// This would be implemented as a [From] trait if rust let us.
pub fn from_raw(
    value: bhyve_api::vcpu_cpuid_entry,
) -> (CpuidIdent, CpuidValues) {
    let subleaf = if value.vce_flags & bhyve_api::VCE_FLAG_MATCH_INDEX != 0 {
        Some(value.vce_index)
    } else {
        None
    };

    (
        CpuidIdent { leaf: value.vce_function, subleaf },
        CpuidValues {
            eax: value.vce_eax,
            ebx: value.vce_ebx,
            ecx: value.vce_ecx,
            edx: value.vce_edx,
        },
    )
}

#[derive(Debug, thiserror::Error)]
pub enum SpecializeError {
    #[error("unsupported cache level")]
    UnsupportedCacheLevel,
    #[error("missing vcpu count")]
    MissingVcpuCount,
    #[error("missing vcpu id")]
    MissingVcpuId,
    #[error("unable to specialize leaf")]
    IncompatibleTopology { leaf: u32, num_vcpu: u32, why: Option<&'static str> },
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
        assert!((vcpuid as usize) < crate::vcpu::MAXCPU);
        Self { vcpuid: Some(vcpuid), ..self }
    }

    /// Specify CPU topology types to render into the specialized [CpuidSet]
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

    /// Specify CPU topology types to clear from the specialized [CpuidSet]
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
    /// render an updated [CpuidSet] reflecting those data.
    pub fn execute(
        self,
        mut set: CpuidSet,
    ) -> Result<CpuidSet, SpecializeError> {
        match set.vendor() {
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
            if let Some(ent) = set.get_mut(CpuidIdent::leaf(1)) {
                // bits 31:24 contain initial APIC ID
                ent.ebx &= !0xff000000;
                ent.ebx |= ((*vcpuid as u32) & 0xff) << 24;
            }
        }

        // logical CPU count (if SMT is enabled)
        if let Some(num_vcpu) = self.num_vcpu.as_ref() {
            if self.has_smt {
                if let Some(ent) = set.get_mut(CpuidIdent::leaf(1)) {
                    ent.edx |= (0x1 << 28);
                    // bits 23:16 contain max IDs for logical CPUs in package
                    ent.ebx &= !0xff0000;
                    ent.ebx |= u32::from(num_vcpu.get()) << 16;
                }
            }
        }

        Ok(set)
    }

    fn fix_amd_cache_topo(
        &self,
        set: &mut CpuidSet,
    ) -> Result<(), SpecializeError> {
        assert!(self.do_cache_topo);
        let num = self.num_vcpu.unwrap().get();
        for ecx in 0..u32::MAX {
            match set.get_mut(CpuidIdent::subleaf(0x8000001d, ecx)) {
                None => break,
                Some(vals) => {
                    // bits 7:5 hold the cache level
                    let visible_count = match ((vals.eax & 0b11100000) >> 5) {
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

    fn fix_cpu_topo(&self, set: &mut CpuidSet) -> Result<(), SpecializeError> {
        // The number of logical threads available to the guest.
        let num_vcpu = self
            .num_vcpu
            .ok_or(SpecializeError::MissingVcpuCount)
            .map(|n| u32::from(n.get()))?;

        // The number of logical processors the guest should see. If we
        // indicate that SMT is enabled, then vCPUs are presented as pairs
        // of sibling threads on vproc-many processors.
        let num_vproc = if self.has_smt {
            // TODO: What if num_vcpu is odd?!
            num_vcpu >> 1
        } else {
            num_vcpu
        };

        for topo in self.cpu_topo_populate.union(&self.cpu_topo_clear) {
            let leaf = *topo as u32;

            if !self.cpu_topo_populate.contains(topo) {
                // We aren't fixing up this leaf, we're just asked to entirely
                // discard it.
                set.remove_leaf(leaf);

                if *topo == TopoKind::Ext1E {
                    // TODO: clear the TopologyExtensions bit in leaf 8000_0001
                    // since we've just discarded the leaf. Change this *after*
                    // the wholesale move to fixed CPUID leaves (Omicron#8728)
                    // so the typical case just never has this bit set and a
                    // change here is not an additional guest-visible change.
                }

                continue;
            }

            if !set.contains_leaf(leaf) {
                // If the leaf isn't present at all, we won't try to specialize
                // it. This lets callers request specializing any/all leaves
                // related to CPU topology without us inventing Intel-only
                // leaves on AMD or AMD-only leaves on Intel.
                continue;
            }

            match topo {
                TopoKind::Std4 => {
                    // Leaf 4 is reserved by AMD, but Intel includes some
                    // topology information here that OSes may use. From the
                    // Intel SDM vol. 2A on
                    //
                    // > Deterministic Cache Parameters Leaf
                    // > (Initial EAX Value = 04H)
                    //
                    // Bits 25-14 are the maximum number of addressable IDs for
                    // logical processors sharing this cache (e.g. "1" for L1
                    // and L2 caches, and "all" for L3)
                    //
                    // Bits 31-26 are the maximum number of addressable IDs for
                    // processor cores in the package. This is constant for all
                    // valid subleaves.

                    // If the number of vCPUs is more than bits 31-26 can
                    // represent, I don't know what to do! This is probably an
                    // all-bits-set-and-use-another-topo-method condition, but
                    // bail out here and demand someone take a look.
                    if num_vproc >= 0b100_0000 {
                        return Err(SpecializeError::IncompatibleTopology {
                            leaf: 4,
                            num_vcpu,
                            why: Some(
                                "Don't know how to set CPUID leaf 4 processor \
                                 count if there are more than 64 processors!",
                            ),
                        });
                    } else if num_vproc == 0 {
                        return Err(SpecializeError::IncompatibleTopology {
                            leaf: 4,
                            num_vcpu,
                            why: Some(
                                "Cannot specialize CPUID leaf 4 for 0 vproc VM",
                            ),
                        });
                    }

                    // Cache types come in any order, but type 0 means there are
                    // no more caches, so iterate and adjust as needed until we
                    // see that.
                    const MAX_REASONABLE_LEVEL: u32 = 0x20;
                    for i in 0..32 {
                        let leaf = set.get(CpuidIdent::subleaf(4, i)).cloned();
                        let Some(mut leaf) = leaf else {
                            // We've reached the end of provided subleaves, so
                            // we're done here.
                            break;
                        };

                        let ty = leaf.eax & 0b11111;
                        let level = (leaf.eax >> 5) & 0b111;

                        if ty == 0 {
                            // "Null" cache. This is not a cache, and there are
                            // no more caches. We're done here.
                            break;
                        }

                        const LEAF4_EAX_VPROC_MASK: u32 = 0xfc_00_00_00;
                        const LEAF4_EAX_VCPU_MASK: u32 = 0x03_ff_c0_00;
                        // Zero out the prior processor core count.
                        leaf.eax &= !LEAF4_EAX_VPROC_MASK;
                        // The processor count is encoded as one less than the
                        // real count (e.g. 0x3f is 64 processors, 0x00 is 1
                        // processor)
                        leaf.eax |= (num_vproc - 1) << 26;

                        // Present L1 and L2 caches as per-thread, L3 is across
                        // the whole VM.
                        if level < 3 {
                            leaf.eax &= !LEAF4_EAX_VCPU_MASK;
                            // And leave that range 0: this means only one
                            // vCPU shares the cache.
                        } else {
                            leaf.eax &= !LEAF4_EAX_VCPU_MASK;
                            let shifted_vcpu = (num_vcpu - 1) << 14;
                            if shifted_vcpu & !LEAF4_EAX_VCPU_MASK != 0 {
                                return Err(
                                    SpecializeError::IncompatibleTopology {
                                        leaf: 4,
                                        num_vcpu,
                                        why: Some("too many vCPUs"),
                                    },
                                );
                            }
                            leaf.eax |= shifted_vcpu;
                        }

                        set.insert(CpuidIdent::subleaf(4, i), leaf)
                            .expect("can put the leaf back where we got it");
                    }
                }
                TopoKind::StdB => {
                    // Queries with invalid ecx will get all-zeroes
                    set.insert(CpuidIdent::leaf(leaf), CpuidValues::default());
                    let Some(vcpuid) = self.vcpuid.map(|id| id as u32) else {
                        return Err(SpecializeError::MissingVcpuId);
                    };

                    if self.has_smt {
                        set.insert(
                            CpuidIdent::subleaf(leaf, 0),
                            CpuidValues {
                                eax: 0x1,
                                ebx: 0x2,
                                ecx: 0x100,
                                edx: vcpuid,
                            },
                        );
                    } else {
                        // We notionally want to insert a leaf like
                        // CpuidValues {
                        //     eax: 0x0,
                        //     ebx: 0x1,
                        //     ecx: 0x100,
                        //     edx: vcpuid,
                        // }
                        // here, but EAX=0 implies the leaf is invalid, rather
                        // than the desired "shift x2APIC ID right by 0 to get
                        // to the topology ID of processor cores"
                        //
                        // The question here is: what does this leaf look like
                        // with hyperthreading disabled? Does this leaf return 0
                        // in EAX implying that it is invalid, or is it 1, with
                        // x2APIC IDs skipping every other entry for the
                        // disabled SMT siblings? Whatever hardware does here is
                        // least likely to surprise guest OSes.
                        return Err(SpecializeError::IncompatibleTopology {
                            leaf: 0xB,
                            num_vcpu,
                            why: Some("Leaf B.1 would have EAX=0"),
                        });
                    }
                    // TODO: Not wholly clear if we should just set this to
                    // `ceil(log2(num_vcpu))` (which should guarantee that the
                    // VM is conceptually one socket) or set this to 7 or 8 like
                    // a "normal" processor.
                    //
                    // Go with 8 for now and error if num_vcpu is above that:
                    // you should trip over this quickly if you've bumped up
                    // VM_MAXCPU and you may have hardware to compare against at
                    // that point.
                    if num_vcpu >= 256 {
                        return Err(SpecializeError::IncompatibleTopology {
                            leaf: 4,
                            num_vcpu,
                            why: Some(
                                "Don't know how to specialize CPUID leaf \
                                       B for more than 256 processors!",
                            ),
                        });
                    }
                    set.insert(
                        CpuidIdent::subleaf(leaf, 1),
                        CpuidValues {
                            eax: 0x8,
                            ebx: num_vcpu,
                            ecx: 0x201,
                            edx: vcpuid,
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
                        CpuidIdent::leaf(leaf),
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
#[derive(
    Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, strum::EnumIter,
)]
pub enum TopoKind {
    /// Leaf 0x4 (legacy Intel cache topology with some CPU information)
    Std4 = 0x4,
    /// Leaf 0xB AMD (and legacy on Intel)
    StdB = 0xb,
    /// Leaf 0x1F (Intel)
    Std1F = 0x1f,
    /// LEAF 0x8000001E (AMD)
    Ext1E = 0x8000001e,
}

impl TopoKind {
    /// Return an iterator of the CPU topology information that Propolis
    /// supports specializing. This is intended as a "reasonable default" for
    /// CPUID profile specialization.
    pub fn supported() -> std::iter::Cloned<std::slice::Iter<'static, Self>> {
        // Topology leaves 1Fh and 8000_001E are partially or entirely TODO, so
        // we can't specialize them.
        [TopoKind::Std4, TopoKind::StdB].as_slice().into_iter().cloned()
    }
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
