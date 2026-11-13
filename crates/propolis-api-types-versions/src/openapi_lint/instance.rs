// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::migration::InstanceMigrateStatusResponse;
use crate::v1;
use crate::v1::components::{backends, devices};
use crate::v1::instance::InstanceProperties;

/// Current state of an Instance.
#[derive(
    Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Creating,
    Starting,
    Running,
    Stopping,
    Stopped,
    Rebooting,
    Migrating,
    Repairing,
    Failed,
    Destroyed,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Instance {
    pub properties: InstanceProperties,
    pub state: InstanceState,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceGetResponse {
    pub instance: Instance,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStateRequested {
    Run,
    Stop,
    Reboot,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorResponse {
    pub gen: u64,
    pub state: InstanceState,
    pub migration: InstanceMigrateStatusResponse,
}

/// An instance spec component that should be replaced during a live migration.
//
// When a caller asks Propolis to initialize via live migration, the target VM
// inherits the migration source's current instance spec. For the most part,
// the target can (and indeed in some cases must) use this spec without
// modifying it; this helps Propolis ensure that guest-visible configuration
// remains unchanged when a VM migrates. However, there are some components
// with no guest-visible state that may need to be reconfigured when a VM
// migrates. These include the following:
//
// - Crucible disks: After migrating, the target Propolis presents itself as a
//   new client of the Crucible downstairs servers backing the VM's disks.
//   Crucible requires the target to present a newer client generation number
//   to allow the target to connect. In a full Oxide deployment, these numbers
//   are managed by the control plane (i.e. it is not safe for Propolis to
//   manage these values directly--new Crucible volume connection information
//   must always come from Nexus).
// - Virtio network devices: Each virtio NIC in the guest needs to bind to a
//   named VNIC object on the host. These names can change when a VM migrates
//   from host to host.
//
// Each component that can be reconfigured this way has a variant in this enum;
// components not in the enum can't be reconfigured during migration. This
// saves the initialization API from having to reason about requests to replace
// a component that can't legally be replaced.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    deny_unknown_fields,
    tag = "component",
    content = "spec",
    rename_all = "snake_case"
)]
pub enum ReplacementComponent {
    MigrationFailureInjector(devices::MigrationFailureInjector),
    CrucibleStorageBackend(backends::CrucibleStorageBackend),
    VirtioNetworkBackend(backends::VirtioNetworkBackend),
}

impl From<InstanceState> for v1::instance::InstanceState {
    fn from(new: InstanceState) -> Self {
        match new {
            InstanceState::Creating => Self::Creating,
            InstanceState::Starting => Self::Starting,
            InstanceState::Running => Self::Running,
            InstanceState::Stopping => Self::Stopping,
            InstanceState::Stopped => Self::Stopped,
            InstanceState::Rebooting => Self::Rebooting,
            InstanceState::Migrating => Self::Migrating,
            InstanceState::Repairing => Self::Repairing,
            InstanceState::Failed => Self::Failed,
            InstanceState::Destroyed => Self::Destroyed,
        }
    }
}

impl From<Instance> for v1::instance::Instance {
    fn from(new: Instance) -> Self {
        let Instance { properties, state } = new;
        Self { properties, state: state.into() }
    }
}

impl From<InstanceGetResponse> for v1::instance::InstanceGetResponse {
    fn from(new: InstanceGetResponse) -> Self {
        let InstanceGetResponse { instance } = new;
        Self { instance: instance.into() }
    }
}

impl From<v1::instance::InstanceStateRequested> for InstanceStateRequested {
    fn from(old: v1::instance::InstanceStateRequested) -> Self {
        match old {
            v1::instance::InstanceStateRequested::Run => Self::Run,
            v1::instance::InstanceStateRequested::Stop => Self::Stop,
            v1::instance::InstanceStateRequested::Reboot => Self::Reboot,
        }
    }
}

impl From<InstanceStateMonitorResponse>
    for v1::instance::InstanceStateMonitorResponse
{
    fn from(new: InstanceStateMonitorResponse) -> Self {
        let InstanceStateMonitorResponse { gen, state, migration } = new;
        Self { gen, state: state.into(), migration: migration.into() }
    }
}

impl From<v1::instance::ReplacementComponent> for ReplacementComponent {
    fn from(old: v1::instance::ReplacementComponent) -> Self {
        match old {
            v1::instance::ReplacementComponent::MigrationFailureInjector(c) => {
                Self::MigrationFailureInjector(c)
            }
            v1::instance::ReplacementComponent::CrucibleStorageBackend(c) => {
                Self::CrucibleStorageBackend(c)
            }
            v1::instance::ReplacementComponent::VirtioNetworkBackend(c) => {
                Self::VirtioNetworkBackend(c)
            }
        }
    }
}
