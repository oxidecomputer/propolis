// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Re-exports of the latest versions of all published types.
//!
//! Business logic should use these re-exports rather than versioned
//! identifiers directly.

pub mod components {
    pub mod backends {
        pub use crate::v1::components::backends::BlobStorageBackend;
        pub use crate::v1::components::backends::CrucibleStorageBackend;
        pub use crate::v1::components::backends::DlpiNetworkBackend;
        pub use crate::v1::components::backends::FileStorageBackend;
        pub use crate::v1::components::backends::VirtioNetworkBackend;
    }

    pub mod board {
        pub use crate::v1::components::board::Board;
        pub use crate::v1::components::board::Chipset;
        pub use crate::v1::components::board::Cpuid;
        pub use crate::v1::components::board::CpuidEntry;
        pub use crate::v1::components::board::GuestHypervisorInterface;
        pub use crate::v1::components::board::HyperVFeatureFlag;
        pub use crate::v1::components::board::I440Fx;
    }

    pub mod devices {
        pub use crate::v1::components::devices::BootOrderEntry;
        pub use crate::v1::components::devices::BootSettings;
        pub use crate::v1::components::devices::MigrationFailureInjector;
        pub use crate::v1::components::devices::P9fs;
        pub use crate::v1::components::devices::PciPciBridge;
        pub use crate::v1::components::devices::QemuPvpanic;
        pub use crate::v1::components::devices::SerialPort;
        pub use crate::v1::components::devices::SerialPortNumber;
        pub use crate::v1::components::devices::SoftNpuP9;
        pub use crate::v1::components::devices::SoftNpuPciPort;
        pub use crate::v1::components::devices::SoftNpuPort;
        pub use crate::v1::components::devices::VirtioDisk;
        pub use crate::v1::components::devices::VirtioNic;

        pub use crate::v3::components::devices::NvmeDisk;
    }
}

pub mod disk {
    pub use crate::v1::disk::InstanceVCRReplace;
    pub use crate::v1::disk::SnapshotRequestPathParams;
    pub use crate::v1::disk::VCRRequestPathParams;
    pub use crate::v1::disk::VolumeStatus;
    pub use crate::v1::disk::VolumeStatusPathParams;
}

pub mod instance {
    pub use crate::v1::instance::ErrorCode;
    pub use crate::v1::instance::Instance;
    pub use crate::v1::instance::InstanceEnsureResponse;
    pub use crate::v1::instance::InstanceGetResponse;
    pub use crate::v1::instance::InstanceMetadata;
    pub use crate::v1::instance::InstanceNameParams;
    pub use crate::v1::instance::InstancePathParams;
    pub use crate::v1::instance::InstanceProperties;
    pub use crate::v1::instance::InstanceState;
    pub use crate::v1::instance::InstanceStateChange;
    pub use crate::v1::instance::InstanceStateMonitorRequest;
    pub use crate::v1::instance::InstanceStateMonitorResponse;
    pub use crate::v1::instance::InstanceStateRequested;
    pub use crate::v1::instance::ReplacementComponent;

    pub use crate::v3::api::InstanceEnsureRequest;
    pub use crate::v3::api::InstanceInitializationMethod;
}

pub mod instance_spec {
    pub use crate::v1::instance_spec::CpuidIdent;
    pub use crate::v1::instance_spec::CpuidValues;
    pub use crate::v1::instance_spec::CpuidVendor;
    pub use crate::v1::instance_spec::PciPath;
    pub use crate::v1::instance_spec::SpecKey;
    pub use crate::v1::instance_spec::VersionedInstanceSpec;

    pub use crate::v2::instance_spec::SmbiosType1Input;

    pub use crate::v3::instance_spec::Component;
    pub use crate::v3::instance_spec::InstanceSpec;
    pub use crate::v3::instance_spec::InstanceSpecGetResponse;
    pub use crate::v3::instance_spec::InstanceSpecStatus;
}

pub mod migration {
    pub use crate::v1::migration::InstanceMigrateInitiateRequest;
    pub use crate::v1::migration::InstanceMigrateInitiateResponse;
    pub use crate::v1::migration::InstanceMigrateStartRequest;
    pub use crate::v1::migration::InstanceMigrateStatusResponse;
    pub use crate::v1::migration::InstanceMigrationStatus;
    pub use crate::v1::migration::MigrationState;
}

pub mod serial {
    pub use crate::v1::serial::InstanceSerialConsoleControlMessage;
    pub use crate::v1::serial::InstanceSerialConsoleHistoryRequest;
    pub use crate::v1::serial::InstanceSerialConsoleHistoryResponse;
    pub use crate::v1::serial::InstanceSerialConsoleStreamRequest;
}
