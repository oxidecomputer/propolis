// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bitflags and constants that provide symbolic names for the various bits in
//! various CPUID leaves.
//!
//! Definitions here are taken from the AMD Architecture Programmer's Manual,
//! volume 3, appendix E (Publication 24594, revision 3.36, March 2024).

pub const STANDARD_BASE_LEAF: u32 = 0;
pub const HYPERVISOR_BASE_LEAF: u32 = 0x4000_0000;
pub const EXTENDED_BASE_LEAF: u32 = 0x8000_0000;

bitflags::bitflags! {
    /// Leaf 1 ecx: instruction feature identifiers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Leaf1Ecx: u32 {
        const SSE3 = 1 << 0;
        const PCLMULQDQ = 1 << 1;
        const MONITOR = 1 << 3;
        const SSSE3 = 1 << 9;
        const FMA = 1 << 12;
        const CMPXCHG16B = 1 << 13;
        const SSE41 = 1 << 19;
        const SSE42 = 1 << 20;
        const X2APIC = 1 << 21;
        const MOVBE = 1 << 22;
        const POPCNT = 1 << 23;
        const AES = 1 << 25;
        const XSAVE = 1 << 26;
        const OSXSAVE = 1 << 27;
        const AVX = 1 << 28;
        const F16C = 1 << 29;
        const RDRAND = 1 << 30;
        const HV_GUEST = 1 << 31;

        const ALL_FLAGS = Self::SSE3.bits() | Self::PCLMULQDQ.bits() |
            Self::MONITOR.bits() | Self::SSSE3.bits() | Self::FMA.bits() |
            Self::CMPXCHG16B.bits() | Self::SSE41.bits() | Self::SSE42.bits() |
            Self::X2APIC.bits() | Self::MOVBE.bits() | Self::POPCNT.bits() |
            Self::AES.bits() | Self::XSAVE.bits() | Self::OSXSAVE.bits() |
            Self::AVX.bits() | Self::F16C.bits() | Self::RDRAND.bits() |
            Self::HV_GUEST.bits();
    }

    /// Leaf 1 edx: Instruction feature identifiers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Leaf1Edx: u32 {
        const FPU = 1 << 0;
        const VME = 1 << 1;
        const DE = 1 << 2;
        const PSE = 1 << 3;
        const TSC = 1 << 4;
        const MSR = 1 << 5;
        const PAE = 1 << 6;
        const MCE = 1 << 7;
        const CMPXCHG8B = 1 << 8;
        const APIC = 1 << 9;
        const SYSENTER = 1 << 11;
        const MTRR = 1 << 12;
        const PGE = 1 << 13;
        const MCA = 1 << 14;
        const CMOV = 1 << 15;
        const PAT = 1 << 16;
        const PSE36 = 1 << 17;
        const CLFLUSH = 1 << 19;
        const MMX = 1 << 23;
        const FXSR = 1 << 24;
        const SSE = 1 << 25;
        const SSE2 = 1 << 26;
        const HTT = 1 << 28;

        const ALL_FLAGS = Self::FPU.bits() | Self::VME.bits() |
            Self::DE.bits() | Self::PSE.bits() | Self::TSC.bits() |
            Self::MSR.bits() | Self::PAE.bits() | Self::MCE.bits() |
            Self::CMPXCHG8B.bits() | Self::APIC.bits() | Self::SYSENTER.bits() |
            Self::MTRR.bits() | Self::PGE.bits() | Self::MCA.bits() |
            Self::CMOV.bits() | Self::PAT.bits() | Self::PSE36.bits() |
            Self::CLFLUSH.bits() | Self::MMX.bits() | Self::FXSR.bits() |
            Self::SSE.bits() | Self::SSE2.bits() | Self::HTT.bits();
    }

    /// Leaf 7 subleaf 0 ebx: instruction feature identifiers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Leaf7Sub0Ebx: u32 {
        const FSGSBASE = 1 << 0;
        const BMI1 = 1 << 3;
        const AVX2 = 1 << 5;
        const SMEP = 1 << 7;
        const BMI2 = 1 << 8;
        const INVPCID = 1 << 10;
        const PQM = 1 << 12;
        const PQE = 1 << 15;
        const RDSEED = 1 << 18;
        const ADX = 1 << 19;
        const SMAP = 1 << 20;
        const CLFLUSHOPT = 1 << 23;
        const CLWB = 1 << 24;
        const SHA = 1 << 29;

        const ALL_FLAGS = Self::FSGSBASE.bits() |
            Self::BMI1.bits() | Self::AVX2.bits() | Self::SMEP.bits() |
            Self::BMI2.bits() | Self::INVPCID.bits() | Self::PQM.bits() |
            Self::PQE.bits() | Self::RDSEED.bits() | Self::ADX.bits() |
            Self::SMAP.bits() | Self::CLFLUSHOPT.bits() | Self::CLWB.bits() |
            Self::SHA.bits();
    }

    /// Leaf 0x8000_0001 ecx: Extended processor feature identifiers.
    ///
    /// NOTE: These definitions are AMD-specific.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AmdExtLeaf1Ecx: u32 {
        const LAHF = 1 << 0;
        const CMP_LEGACY = 1 << 1;
        const SVM = 1 << 2;
        const EXT_APIC_SPACE = 1 << 3;
        const ALT_MOV_CR8 = 1 << 4;
        const ABM = 1 << 5;
        const SSE4A = 1 << 6;
        const MISALIGN_SSE = 1 << 7;
        const THREED_NOW_PREFETCH = 1 << 8;
        const OSVW = 1 << 9;
        const IBS = 1 << 10;
        const XOP = 1 << 11;
        const SKINIT = 1 << 12;
        const WDT = 1 << 13;
        const LWP = 1 << 15;
        const FMA4 = 1 << 16;
        const TCE = 1 << 17;
        const TBM = 1 << 21;
        const TOPOLOGY_EXT = 1 << 22;
        const PMC_EXT_CORE = 1 << 23;
        const PMC_EXT_NB = 1 << 24;
        const DATA_ACCESS_BP = 1 << 26;
        const PERF_TSC = 1 << 27;
        const PMC_EXT_LLC = 1 << 28;
        const MONITORX = 1 << 29;
        const DATA_BP_ADDR_MASK_EXT = 1 << 30;

        const ALL_FLAGS = Self::LAHF.bits() | Self::CMP_LEGACY.bits() |
            Self::SVM.bits() | Self::EXT_APIC_SPACE.bits() |
            Self::ALT_MOV_CR8.bits() | Self::ABM.bits() | Self::SSE4A.bits() |
            Self::MISALIGN_SSE.bits() | Self::THREED_NOW_PREFETCH.bits() |
            Self::OSVW.bits() | Self::IBS.bits() | Self::XOP.bits() |
            Self::SKINIT.bits() | Self::WDT.bits() | Self::LWP.bits() |
            Self::FMA4.bits() | Self::TCE.bits() | Self::TBM.bits() |
            Self::TOPOLOGY_EXT.bits() | Self::PMC_EXT_CORE.bits() |
            Self::PMC_EXT_NB.bits() | Self::DATA_ACCESS_BP.bits() |
            Self::PERF_TSC.bits() | Self::PMC_EXT_LLC.bits() |
            Self::MONITORX.bits() | Self::DATA_BP_ADDR_MASK_EXT.bits();
    }

    /// Leaf 0x8000_0001 edx: Extended processor feature identifiers.
    ///
    /// NOTE: These definitions are AMD-specific.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AmdExtLeaf1Edx: u32 {
        const FPU = Leaf1Edx::FPU.bits();
        const VME = Leaf1Edx::VME.bits();
        const DE = Leaf1Edx::DE.bits();
        const PSE = Leaf1Edx::PSE.bits();
        const TSC = Leaf1Edx::TSC.bits();
        const MSR = Leaf1Edx::MSR.bits();
        const PAE = Leaf1Edx::PAE.bits();
        const MCE = Leaf1Edx::MCE.bits();
        const CMPXCHG8B = Leaf1Edx::CMPXCHG8B.bits();
        const APIC = Leaf1Edx::APIC.bits();
        const SYSCALL = 1 << 11;
        const MTRR = Leaf1Edx::MTRR.bits();
        const PGE = Leaf1Edx::PGE.bits();
        const MCA = Leaf1Edx::MCA.bits();
        const CMOV = Leaf1Edx::CMOV.bits();
        const PAT = Leaf1Edx::PAT.bits();
        const PSE36 = Leaf1Edx::PSE36.bits();
        const NX = 1 << 20;
        const MMX_EXT = 1 << 22;
        const MMX = 1 << 23;
        const FXSAVE = 1 << 24;
        const FXSAVE_OPT = 1 << 25;
        const GB_PAGE = 1 << 26;
        const RDTSCP = 1 << 27;
        const LONG_MODE = 1 << 29;
        const THREED_NOW_EXT = 1 << 30;
        const THREED_NOW = 1 << 31;

        const ALL_FLAGS = Self::FPU.bits() | Self::VME.bits() |
            Self::DE.bits() | Self::PSE.bits() | Self::TSC.bits() |
            Self::MSR.bits() | Self::PAE.bits() | Self::MCE.bits() |
            Self::CMPXCHG8B.bits() | Self::APIC.bits() | Self::SYSCALL.bits() |
            Self::MTRR.bits() | Self::PGE.bits() | Self::MCA.bits() |
            Self::CMOV.bits() | Self::PAT.bits() | Self::PSE36.bits() |
            Self::NX.bits() | Self::MMX_EXT.bits() | Self::MMX.bits() |
            Self::FXSAVE.bits() | Self::FXSAVE_OPT.bits() |
            Self::GB_PAGE.bits() | Self::RDTSCP.bits() |
            Self::LONG_MODE.bits() | Self::THREED_NOW_EXT.bits() |
            Self::THREED_NOW.bits();
    }

    /// Leaf 0x8000_001D eax: Cache topology information.
    ///
    /// NOTE: These definitions are AMD-specific.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AmdExtLeaf1DEax: u32 {
        const NUM_SHARING_CACHE_MASK = (0xFFF << 14);
        const FULLY_ASSOCIATIVE = 1 << 9;
        const SELF_INITIALIZATION = 1 << 8;
        const CACHE_LEVEL_MASK = (0x7 << 5);
        const CACHE_TYPE_MASK = 0x1F;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmdExtLeaf1DCacheType {
    Null,
    Data,
    Instruction,
    Unified,
    Reserved,
}

impl AmdExtLeaf1DCacheType {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl TryFrom<u32> for AmdExtLeaf1DCacheType {
    type Error = ();

    /// Returns the leaf 0x8000001D cache type corresponding to the supplied
    /// value, or an error if the supplied value cannot be represented in 5 bits
    /// (the width of the cache type field in leaf 0x8000001D eax).
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Null),
            1 => Ok(Self::Data),
            2 => Ok(Self::Instruction),
            3 => Ok(Self::Unified),
            4..=0x1F => Ok(Self::Reserved),
            _ => Err(()),
        }
    }
}

impl AmdExtLeaf1DEax {
    pub fn cache_type(&self) -> AmdExtLeaf1DCacheType {
        let bits = (*self & Self::CACHE_TYPE_MASK).bits();
        AmdExtLeaf1DCacheType::try_from(bits)
            .expect("invalid bits were already masked")
    }
}
