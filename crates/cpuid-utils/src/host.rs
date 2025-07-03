// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bhyve_api::{VmmCtlFd, VmmFd};
use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor};
use thiserror::Error;

use crate::{
    bits::{
        AmdExtLeaf1DCacheType, AmdExtLeaf1DEax, Leaf1Ecx, Leaf7Sub0Ebx,
        EXTENDED_BASE_LEAF, STANDARD_BASE_LEAF,
    },
    CpuidMapInsertError, CpuidSet,
};

#[derive(Debug, Error)]
pub enum GetHostCpuidError {
    #[error("failed to insert into the CPUID map")]
    CpuidInsertFailed(#[from] CpuidMapInsertError),

    #[error("CPUID vendor not recognized: {0}")]
    VendorNotRecognized(&'static str),

    #[error("I/O error from bhyve API")]
    BhyveError(#[from] std::io::Error),
}

/// A wrapper around a handle to a bhyve VM that can be used to query bhyve's
/// default CPUID values.
struct Vm(bhyve_api::VmmFd);

impl Vm {
    fn new() -> Result<Self, GetHostCpuidError> {
        let name = format!("cpuid-gen-{}", std::process::id());
        let mut req = bhyve_api::vm_create_req::new(name.as_bytes())
            .expect("valid VM name");

        let ctl = VmmCtlFd::open()?;
        let _ = unsafe { ctl.ioctl(bhyve_api::VMM_CREATE_VM, &mut req) }?;

        let vm = match VmmFd::open(&name) {
            Ok(vm) => vm,
            Err(e) => {
                // Attempt to manually destroy the VM if we cannot open it
                let _ = ctl.vm_destroy(name.as_bytes());
                return Err(e.into());
            }
        };

        Ok(Self(vm))
    }

    fn query(
        &self,
        vlc_eax: u32,
        vlc_ecx: u32,
    ) -> Result<CpuidValues, GetHostCpuidError> {
        let mut data = bhyve_api::vm_legacy_cpuid {
            vlc_eax,
            vlc_ecx,
            ..Default::default()
        };
        unsafe { self.0.ioctl(bhyve_api::VM_LEGACY_CPUID, &mut data) }?;
        Ok(CpuidValues {
            eax: data.vlc_eax,
            ebx: data.vlc_ebx,
            ecx: data.vlc_ecx,
            edx: data.vlc_edx,
        })
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.0.ioctl_usize(bhyve_api::VM_DESTROY_SELF, 0);
    }
}

/// Queries the supplied CPUID leaf on the caller's machine.
#[cfg(target_arch = "x86_64")]
pub fn query(leaf: CpuidIdent) -> CpuidValues {
    unsafe {
        core::arch::x86_64::__cpuid_count(leaf.leaf, leaf.subleaf.unwrap_or(0))
    }
    .into()
}

#[cfg(not(target_arch = "x86_64"))]
pub fn query(leaf: CpuidIdent) -> CpuidValues {
    panic!("host CPUID queries only work on x86-64 hosts")
}

fn collect_cpuid(
    query: &impl Fn(u32, u32) -> Result<CpuidValues, GetHostCpuidError>,
) -> Result<CpuidSet, GetHostCpuidError> {
    let mut set = CpuidSet::default();

    // Enumerate standard leaves and copy their values into the output set.
    //
    // Note that enumeration order matters here: leaf D is only treated as
    // having subleaves if leaf 1 indicates support for XSAVE.
    let std = query(STANDARD_BASE_LEAF, 0)?;
    set.vendor = CpuidVendor::try_from(std)
        .map_err(GetHostCpuidError::VendorNotRecognized)?;
    let mut xsave_supported = false;
    for leaf in 0..=std.eax {
        match leaf {
            0x1 => {
                let data = query(leaf, 0)?;
                xsave_supported = (Leaf1Ecx::from_bits_retain(data.ecx)
                    & Leaf1Ecx::XSAVE)
                    .bits()
                    != 0;
                set.insert(CpuidIdent::leaf(leaf), data)?;
            }
            // Leaf 0x7 subleaf 0 eax indicates the total number of leaf-7
            // subleaves.
            0x7 => {
                let mut data = query(leaf, 0)?;

                // Leaf 0x7 EBX bits 12 and 15 indicate PQM and PQE support on
                // AMD CPUs, and aspects of RDT support on Intel CPUs. In both
                // cases, if the bits are set, leaves 0xF and 0x10 are actually
                // subleaves with further capability information for the
                // corresponding features. We don't support passing these
                // features along, so mask out the bits.
                data.ebx &= !(Leaf7Sub0Ebx::PQM | Leaf7Sub0Ebx::PQE).bits();

                set.insert(CpuidIdent::subleaf(leaf, 0), data)?;
                for subleaf in 1..=data.eax {
                    let sub_data = query(leaf, subleaf)?;
                    set.insert(CpuidIdent::subleaf(leaf, subleaf), sub_data)?;
                }
            }
            // Leaf 0xB contains CPU topology information. Although this leaf
            // can theoretically support many levels of information, bhyve
            // supports only subleaves 0 and 1, so just query those without
            // trying to reason about exactly how many topology nodes the host
            // exposes.
            0xB => {
                set.insert(CpuidIdent::subleaf(leaf, 0), query(leaf, 0)?)?;
                set.insert(CpuidIdent::subleaf(leaf, 1), query(leaf, 1)?)?;
            }
            // Leaf 0xD contains information about extended processor state.
            0xD if xsave_supported => {
                let data = query(leaf, 0)?;
                set.insert(CpuidIdent::subleaf(leaf, 0), data)?;

                // Subleaf 0 edx:eax contains a 64-bit mask indicating what
                // features requiring extended state can be enabled in xcr0.
                let xcr0_bits =
                    u64::from(data.eax) | (u64::from(data.edx) << 32);

                let data = query(leaf, 1)?;
                set.insert(CpuidIdent::subleaf(leaf, 1), data)?;

                // Subleaf 1 edx:ecx contains a 64-bit mask indicating what
                // features requiring extended state can be enabled in the
                // IA32_XSS MSR.
                let xss_bits =
                    u64::from(data.ecx) | (u64::from(data.edx) << 32);

                // Subleaves 2 through 63 are valid if the corresponding mask
                // bit is set either in the xcr0 mask returned by subleaf 0 or
                // the XSS mask returned by subleaf 1.
                for ecx in 2..64 {
                    if (1 << ecx) & (xcr0_bits | xss_bits) == 0 {
                        continue;
                    }

                    set.insert(
                        CpuidIdent::subleaf(leaf, ecx),
                        query(leaf, ecx)?,
                    )?;
                }
            }
            // Leaf 0xF describes Platform QoS Monitoring ("PQM").
            0xF => {
                // Since we're hiding PQM, provide an empty leaf here.
                set.insert(CpuidIdent::leaf(leaf), CpuidValues::default())?;
            }
            // Leaf 0x10 describes Platform QoS Enforcement ("PQE").
            0x10 => {
                // Since we're hiding PQE, provide an empty leaf here.
                set.insert(CpuidIdent::leaf(leaf), CpuidValues::default())?;
            }
            _ => {
                set.insert(CpuidIdent::leaf(leaf), query(leaf, 0)?)?;
            }
        }
    }

    let extended = query(EXTENDED_BASE_LEAF, 0)?;
    for leaf in EXTENDED_BASE_LEAF..=extended.eax {
        match leaf {
            0x8000_001D => {
                for subleaf in 0..=u32::MAX {
                    let data = query(leaf, subleaf)?;
                    let eax = AmdExtLeaf1DEax::from_bits_retain(data.eax);
                    if eax.cache_type() == AmdExtLeaf1DCacheType::Null {
                        break;
                    }

                    set.insert(CpuidIdent::subleaf(leaf, subleaf), data)?;
                }
            }
            // Leaf 0x8000_0020 has extended AMD PQM/PQE feature information.
            0x8000_0020 => {
                // Since we're hiding PQM and PQE, provide an empty leaf here.
                // Features described in this leaf may relate to PQM or PQE, and
                // it's not immediately clear what to present if only one
                // feature is present.
                //
                // In any case, no CPU exists that supports only one of PQM or
                // PQE.
                set.insert(CpuidIdent::leaf(leaf), CpuidValues::default())?;
            }
            // Leaf 0x8000_0026 has extended AMD CPU topology information.
            0x8000_0026 => {
                // The vCPU topology is unrelated to the underlying hardware
                // topology, so hide the real topology for the time being.
                //
                // Hidden or not, it would be erroneous to use the default
                // querying behavior for this leaf. Querying the first host
                // subleaf and reporting leaf 0x8000_0026 as a normal leaf with
                // that data would result in that first topology subleaf being
                // provided for a guest's query of any subleaf. The APM's
                // guidance on how to query extended CPU topology would then
                // lead software to loop over all values of ECX querying
                // subleaves to enumerate a bogus topology:
                //
                // > The topology level is selected by the value passed to the
                // > instruction in ECX. To discover the topology of a system,
                // > software should execute CPUID Fn8000_0026 with increasing
                // > ECX values, starting with a value of zero, until the
                // > returned hierarchy level type (CPUID
                // > Fn8000_0026_ECX[LevelType]) is equal to zero
                set.insert(CpuidIdent::leaf(leaf), CpuidValues::default())?;
            }
            _ => {
                set.insert(CpuidIdent::leaf(leaf), query(leaf, 0)?)?;
            }
        }
    }

    Ok(set)
}

/// A possible source of CPUID information.
#[derive(Clone, Copy)]
pub enum CpuidSource {
    /// Create a temporary VM and ask bhyve what values it would return if one
    /// of its CPUs executed CPUID.
    BhyveDefault,

    /// Execute the CPUID instruction on the host.
    HostCpu,
}

/// Queries the supplied `source` for a "complete" set of CPUID values, i.e., a
/// full set of leaves and subleaves describing the CPU platform the selected
/// source exposes.
pub fn query_complete(
    source: CpuidSource,
) -> Result<CpuidSet, GetHostCpuidError> {
    let query: Box<dyn Fn(u32, u32) -> Result<_, _>> = match source {
        CpuidSource::BhyveDefault => {
            let vm = Vm::new()?;
            Box::new(move |eax, ecx| vm.query(eax, ecx))
        }
        CpuidSource::HostCpu => {
            Box::new(|eax, ecx| Ok(query(CpuidIdent::subleaf(eax, ecx))))
        }
    };

    collect_cpuid(&query)
}
