// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

//! Types for tracking statistics about virtual machine instances.

// Propolis is built in a variety of configurations, including checks and tests
// run on non-illumos machines where kstats are meaningless. This is a big
// hammer, but a large number of the values in this module are not referenced in
// those configurations, and so this is more straightfoward than littering the
// code with cfg directives.
#![cfg_attr(any(test, not(target_os = "illumos")), allow(dead_code))]

use chrono::{DateTime, Utc};
use oximeter::{
    types::Cumulative, FieldType, FieldValue, Metric, Sample, Target,
};
use std::collections::BTreeMap;
use uuid::Uuid;

#[cfg(all(not(test), target_os = "illumos"))]
mod kstat_types {
    pub use kstat_rs::Data;
    pub use kstat_rs::Kstat;
    pub use kstat_rs::NamedData;
    pub use oximeter_instruments::kstat::hrtime_to_utc;
    pub use oximeter_instruments::kstat::ConvertNamedData;
    pub use oximeter_instruments::kstat::Error;
    pub use oximeter_instruments::kstat::KstatList;
    pub use oximeter_instruments::kstat::KstatTarget;
}

// Mock the relevant subset of `kstat-rs` types needed for tests.
#[cfg(not(all(not(test), target_os = "illumos")))]
mod kstat_types {
    use chrono::DateTime;
    use chrono::Utc;

    #[derive(Debug)]
    pub enum Data<'a> {
        Named(Vec<Named<'a>>),
        #[allow(dead_code)]
        Null,
    }

    #[derive(Debug)]
    pub enum NamedData<'a> {
        UInt32(u32),
        UInt64(u64),
        String(&'a str),
    }

    #[derive(Debug)]
    pub struct Kstat<'a> {
        pub ks_module: &'a str,
        pub ks_instance: i32,
        pub ks_name: &'a str,
        pub ks_snaptime: i64,
    }

    #[derive(Debug)]
    pub struct Named<'a> {
        pub name: &'a str,
        pub value: NamedData<'a>,
    }

    pub trait ConvertNamedData {
        fn as_i32(&self) -> Result<i32, Error>;
        fn as_u32(&self) -> Result<u32, Error>;
        fn as_i64(&self) -> Result<i64, Error>;
        fn as_u64(&self) -> Result<u64, Error>;
    }

    impl<'a> ConvertNamedData for NamedData<'a> {
        fn as_i32(&self) -> Result<i32, Error> {
            unimplemented!()
        }

        fn as_u32(&self) -> Result<u32, Error> {
            if let NamedData::UInt32(x) = self {
                Ok(*x)
            } else {
                panic!()
            }
        }

        fn as_i64(&self) -> Result<i64, Error> {
            unimplemented!()
        }

        fn as_u64(&self) -> Result<u64, Error> {
            if let NamedData::UInt64(x) = self {
                Ok(*x)
            } else {
                panic!()
            }
        }
    }

    #[derive(thiserror::Error, Clone, Debug)]
    pub enum Error {
        #[error("No such kstat")]
        NoSuchKstat,
        #[error("Expected a named kstat")]
        ExpectedNamedKstat,
        #[error("Metrics error")]
        Metrics(#[from] oximeter::MetricsError),
    }

    pub fn hrtime_to_utc(_: i64) -> Result<DateTime<Utc>, Error> {
        Ok(Utc::now())
    }
}

pub use kstat_types::*;

/// A single virtual machine instance.
#[derive(Clone, Debug)]
pub struct VirtualMachine {
    /// The silo to which the instance belongs.
    pub silo_id: Uuid,
    /// The project to which the instance belongs.
    pub project_id: Uuid,
    /// The ID of the instance.
    pub instance_id: Uuid,

    // This field is not published as part of the target field definitions. It
    // is needed because the hypervisor currently creates kstats for each vCPU,
    // regardless of whether they're activated. There is no way to tell from
    // userland today which vCPU kstats are "real". We include this value here,
    // and implement `oximeter::Target` manually, so that this field is not
    // published as a field on the timeseries.
    n_vcpus: u32,

    // Same for this field, not published as part of the target, but used to
    // find the right kstats.
    vm_name: String,
}

impl VirtualMachine {
    /// Return the number of vCPUs in this VM.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn n_vcpus(&self) -> u32 {
        self.n_vcpus
    }
}

impl From<&propolis_api_types::InstanceProperties> for VirtualMachine {
    fn from(properties: &propolis_api_types::InstanceProperties) -> Self {
        Self {
            silo_id: properties.metadata.silo_id,
            project_id: properties.metadata.project_id,
            instance_id: properties.id,
            n_vcpus: properties.vcpus.into(),
            vm_name: properties.vm_name(),
        }
    }
}

impl Target for VirtualMachine {
    fn name(&self) -> &'static str {
        "virtual_machine"
    }

    fn field_names(&self) -> &'static [&'static str] {
        &["silo_id", "project_id", "instance_id"]
    }

    fn field_types(&self) -> Vec<FieldType> {
        vec![FieldType::Uuid, FieldType::Uuid, FieldType::Uuid]
    }

    fn field_values(&self) -> Vec<FieldValue> {
        vec![
            self.silo_id.into(),
            self.project_id.into(),
            self.instance_id.into(),
        ]
    }
}

/// Metric tracking vCPU usage by state.
#[derive(Clone, Debug, Metric)]
pub struct VcpuUsage {
    /// The vCPU ID.
    pub vcpu_id: u32,
    /// The state of the vCPU.
    pub state: String,
    /// The cumulative time spent in this state, in nanoseconds.
    pub datum: Cumulative<u64>,
}

// The kstats tracking occupancy in the various microstates have specific names.
// We avoid exposing that in the oximeter samples, and instead map the micro
// state names into our own set of state names.
//
// This returns the public named state to which a microstate maps, if any.
//
// See https://github.com/illumos/illumos-gate/blob/297b0dea3578abea9526441154d0dfa29697c891/usr/src/uts/intel/io/vmm/vmm_sol_dev.c#L2815
// for a definition of these states.
fn kstat_microstate_to_state_name(ustate: &str) -> Option<&'static str> {
    match ustate {
        "time_emu_kern" | "time_emu_user" => Some(OXIMETER_EMULATION_STATE),
        "time_run" => Some(OXIMETER_RUN_STATE),
        "time_init" | "time_idle" => Some(OXIMETER_IDLE_STATE),
        "time_sched" => Some(OXIMETER_WAITING_STATE),
        _ => None,
    }
}

// The definitions of each oximeter-level microstate we track.
const OXIMETER_EMULATION_STATE: &str = "emulation";
const OXIMETER_RUN_STATE: &str = "run";
const OXIMETER_IDLE_STATE: &str = "idle";
const OXIMETER_WAITING_STATE: &str = "waiting";
const OXIMETER_STATES: [&str; 4] = [
    OXIMETER_EMULATION_STATE,
    OXIMETER_RUN_STATE,
    OXIMETER_IDLE_STATE,
    OXIMETER_WAITING_STATE,
];

/// The number of expected vCPU microstates we track.
///
/// This is used to preallocate data structures for holding samples, and to
/// limit the number of samples in the `KstatSampler`, if it is not pulled
/// quickly enough by `oximeter`.
pub(crate) const N_VCPU_MICROSTATES: u32 = OXIMETER_STATES.len() as _;

// The name of the kstat module containing virtual machine kstats.
const VMM_KSTAT_MODULE_NAME: &str = "vmm";

// The name of the kstat with virtual machine metadata (VM name currently).
const VM_KSTAT_NAME: &str = "vm";

// The named kstat holding the virtual machine's name. This is currently the
// UUID assigned by the control plane to the virtual machine instance.
const VM_NAME_KSTAT: &str = "vm_name";

// The name of kstat containing vCPU usage data.
const VCPU_KSTAT_PREFIX: &str = "vcpu";

#[cfg(all(not(test), target_os = "illumos"))]
impl KstatTarget for VirtualMachine {
    // The VMM kstats are organized like so:
    //
    // - module: vmm
    // - instance: a kernel-assigned integer
    // - name: vm -> generic VM info, vcpuX -> info for each vCPU
    //
    // At this part of the code, we don't have that kstat instance, only the
    // virtual machine instance's control plane UUID. However, the VM's "name"
    // is assigned to be that control plane UUID in the hypervisor. See
    // https://github.com/oxidecomputer/propolis/blob/759bf4a19990404c135e608afbe0d38b70bfa370/bin/propolis-server/src/lib/vm/mod.rs#L420
    // for the current code which does that.
    //
    // That means we need to indicate interest in both the `vm` and `vcpuX`
    // kstats for any instance, and then filter to the right instance in the
    // `to_samples()` method below, because interest is defined on each
    // individual kstat.
    fn interested(&self, kstat: &Kstat<'_>) -> bool {
        kstat.ks_module == VMM_KSTAT_MODULE_NAME
    }

    fn to_samples(
        &self,
        kstats: KstatList<'_, '_>,
    ) -> Result<Vec<Sample>, Error> {
        // First, we need to map the instance's control plane UUID to the kstat
        // instance. We'll find this through the `vmm:<instance>:vm:vm_name`
        // kstat, which lists the instance's UUID as its name. The
        // `VirtualMachine` target stores that internally as the `vm_name`
        // field.
        //
        // Note that if this code is run from within a Propolis zone, there is
        // exactly one `vmm` kstat instance in any case.
        let instance = kstats
            .iter()
            .find_map(|(_, kstat, data)| {
                kstat_instance_from_instance_id(kstat, data, &self.vm_name)
            })
            .ok_or_else(|| Error::NoSuchKstat)?;

        // Armed with the kstat instance, find all relevant metrics related to
        // this particular VM. For now, we produce only vCPU usage metrics, but
        // others may be chained in the future.
        let vcpu_stats = kstats.iter().filter(|(_, kstat, _)| {
            // Filter out those that don't match our kstat instance.
            if kstat.ks_instance != instance {
                return false;
            }

            // Filter out those which are neither a vCPU stat of any kind, nor
            // for one of the vCPU IDs we know to be active.
            let Some(suffix) = kstat.ks_name.strip_prefix(VCPU_KSTAT_PREFIX)
            else {
                return false;
            };
            let Ok(vcpu_id) = suffix.parse::<u32>() else {
                return false;
            };
            vcpu_id < self.n_vcpus
        });
        produce_vcpu_usage(self, vcpu_stats)
    }
}

// Given a kstat and an instance's ID, return the kstat instance if it matches.
fn kstat_instance_from_instance_id(
    kstat: &Kstat<'_>,
    data: &Data<'_>,
    instance_id: &str,
) -> Option<i32> {
    // Filter out anything that's not a `vmm:<instance>:vm` named kstat.
    if kstat.ks_module != VMM_KSTAT_MODULE_NAME {
        return None;
    }
    if kstat.ks_name != VM_KSTAT_NAME {
        return None;
    }
    let Data::Named(named) = data else {
        return None;
    };

    // Return the instance if the `vm_name` kstat matches our instance UUID.
    if named.iter().any(|nd| {
        if nd.name != VM_NAME_KSTAT {
            return false;
        }
        let NamedData::String(name) = &nd.value else {
            return false;
        };
        instance_id == *name
    }) {
        return Some(kstat.ks_instance);
    }
    None
}

// Produce `Sample`s for the `VcpuUsage` metric from the relevant kstats.
fn produce_vcpu_usage<'a>(
    vm: &'a VirtualMachine,
    vcpu_stats: impl Iterator<Item = &'a (DateTime<Utc>, Kstat<'a>, Data<'a>)> + 'a,
) -> Result<Vec<Sample>, Error> {
    let mut out =
        Vec::with_capacity(vm.n_vcpus as usize * N_VCPU_MICROSTATES as usize);
    for (creation_time, kstat, data) in vcpu_stats {
        let Data::Named(named) = data else {
            return Err(Error::ExpectedNamedKstat);
        };
        let snapshot_time = hrtime_to_utc(kstat.ks_snaptime)?;

        // Find the vCPU ID, from the `vmm:<instance>:vcpuX:vcpu` named kstat.
        let vcpu_id = named
            .iter()
            .find_map(|named| {
                if named.name == VCPU_KSTAT_PREFIX {
                    named.value.as_u32().ok()
                } else {
                    None
                }
            })
            .ok_or(Error::NoSuchKstat)?;

        // We track each vCPU microstate starting with `time_`, and map them
        // into our own definitions of the vCPU states. We need to aggregate all
        // the occupancy times from the microstates that map to the same public
        // state.
        let mut occupancy_by_state = BTreeMap::new();
        for nv in named.iter() {
            // Skip kstats that are not known microstate names.
            let Some(state) = kstat_microstate_to_state_name(nv.name) else {
                continue;
            };

            // Get the current summed state occupancy, or insert one with 0.
            let datum =
                occupancy_by_state.entry(state.to_string()).or_insert_with(
                    || Cumulative::with_start_time(*creation_time, 0),
                );
            *datum += nv.value.as_u64()?;
        }

        // Now convert the aggregated occupancy times into samples.
        for (state, datum) in occupancy_by_state.into_iter() {
            let metric = VcpuUsage { vcpu_id, state, datum };
            let sample =
                Sample::new_with_timestamp(snapshot_time, vm, &metric)?;
            out.push(sample);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod test {
    use super::kstat_instance_from_instance_id;
    use super::kstat_microstate_to_state_name;
    use super::produce_vcpu_usage;
    use super::Data;
    use super::Kstat;
    use super::Named;
    use super::NamedData;
    use super::Utc;
    use super::VcpuUsage;
    use super::VirtualMachine;
    use super::VCPU_KSTAT_PREFIX;
    use super::VMM_KSTAT_MODULE_NAME;
    use super::VM_KSTAT_NAME;
    use super::VM_NAME_KSTAT;
    use crate::stats::virtual_machine::N_VCPU_MICROSTATES;
    use crate::stats::virtual_machine::OXIMETER_EMULATION_STATE;
    use crate::stats::virtual_machine::OXIMETER_IDLE_STATE;
    use crate::stats::virtual_machine::OXIMETER_RUN_STATE;
    use crate::stats::virtual_machine::OXIMETER_WAITING_STATE;
    use oximeter::types::Cumulative;
    use oximeter::Datum;
    use oximeter::FieldValue;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn test_virtual_machine() -> VirtualMachine {
        const INSTANCE_ID: Uuid =
            uuid::uuid!("96d6ec78-543a-4188-830e-37e2a0eeff16");
        const PROJECT_ID: Uuid =
            uuid::uuid!("7b61df02-0794-4b37-93bc-89f03c7289ca");
        const SILO_ID: Uuid =
            uuid::uuid!("6a4bd4b6-e9aa-44d1-b616-399d48baa173");
        VirtualMachine {
            silo_id: SILO_ID,
            project_id: PROJECT_ID,
            instance_id: INSTANCE_ID,
            n_vcpus: 4,
            vm_name: INSTANCE_ID.to_string(),
        }
    }

    fn test_usage() -> VcpuUsage {
        VcpuUsage {
            state: "run".to_string(),
            vcpu_id: 0,
            datum: Cumulative::new(100),
        }
    }

    #[test]
    fn test_kstat_instance_from_instance_id() {
        let ks = Kstat {
            ks_module: VMM_KSTAT_MODULE_NAME,
            ks_instance: 0,
            ks_name: VM_KSTAT_NAME,
            ks_snaptime: 1,
        };
        const INSTANCE_ID: &str = "db198b43-2dee-4b4b-8a68-24cb4c0d6ec8";
        let data = Data::Named(vec![Named {
            name: VM_NAME_KSTAT,
            value: NamedData::String(INSTANCE_ID),
        }]);

        assert_eq!(
            kstat_instance_from_instance_id(&ks, &data, INSTANCE_ID)
                .expect("Should have matched the instance ID"),
            ks.ks_instance,
        );

        let data = Data::Named(vec![Named {
            name: VM_NAME_KSTAT,
            value: NamedData::String("something-else"),
        }]);
        assert!(
            kstat_instance_from_instance_id(&ks, &data, INSTANCE_ID).is_none(),
            "Should not have matched an instance ID"
        );
    }

    fn vcpu_state_kstats<'a>() -> (Kstat<'a>, Data<'a>) {
        let ks = Kstat {
            ks_module: VMM_KSTAT_MODULE_NAME,
            ks_instance: 0,
            ks_name: "vcpu0",
            ks_snaptime: 1,
        };
        let data = Data::Named(vec![
            Named { name: VCPU_KSTAT_PREFIX, value: NamedData::UInt32(0) },
            // There are three ustates, but the first two are aggregated.
            Named { name: "time_init", value: NamedData::UInt64(1) },
            Named { name: "time_idle", value: NamedData::UInt64(1) },
            Named { name: "time_run", value: NamedData::UInt64(2) },
        ]);
        (ks, data)
    }

    #[test]
    fn test_produce_vcpu_usage() {
        let (ks, data) = vcpu_state_kstats();
        let kstats = [(Utc::now(), ks, data)];
        let samples =
            produce_vcpu_usage(&test_virtual_machine(), kstats.iter())
                .expect("Should have produced samples");
        assert_eq!(
            samples.len(),
            2,
            "Should have samples for 'run' and 'idle' states"
        );
        for ((sample, state), x) in samples
            .iter()
            .zip([OXIMETER_IDLE_STATE, OXIMETER_RUN_STATE])
            .zip([2, 2])
        {
            let st = sample
                .fields()
                .iter()
                .find_map(|f| {
                    if f.name == "state" {
                        let FieldValue::String(state) = &f.value else {
                            panic!("Expected a string field");
                        };
                        Some(state.clone())
                    } else {
                        None
                    }
                })
                .expect("expected a field with name \"state\"");
            assert_eq!(st, state, "Found an incorrect vCPU state");
            let Datum::CumulativeU64(inner) = sample.measurement.datum() else {
                panic!("Expected a cumulativeu64 datum");
            };
            assert_eq!(inner.value(), x);
        }
    }

    // Sanity check that the mapping from lower-level `kstat` vCPU microstates
    // to the higher-level states we report to `oximeter` do not change.
    #[test]
    fn test_consistent_kstat_to_oximeter_microstate_mapping() {
        // Build our expected mapping from kstat-to-oximeter states.
        //
        // For each oximeter state, we pretend to have observed the kstat-level
        // microstates that go into it some number of times. We then check that
        // the number of actual observed mapped states (for each kstat-level
        // one) is matches our expectation.
        //
        // For example, the `time_emu_{kern,user}` states map to the
        // `"emulation"` state. If we observe 1 and 2 of those, respectively, we
        // should have a total of 3 observations of the `"emulation"` state.
        let mut expected_states = BTreeMap::new();
        expected_states.insert(
            OXIMETER_EMULATION_STATE,
            vec![("time_emu_kern", 1usize), ("time_emu_user", 2)],
        );
        expected_states.insert(
            OXIMETER_RUN_STATE,
            vec![("time_run", 4)], // Not equal to sum above
        );
        expected_states.insert(
            OXIMETER_IDLE_STATE,
            vec![("time_init", 5), ("time_idle", 6)],
        );
        expected_states.insert(OXIMETER_WAITING_STATE, vec![("time_sched", 7)]);
        assert_eq!(
            expected_states.len() as u32,
            N_VCPU_MICROSTATES,
            "Expected set of oximeter states does not match const",
        );

        // "Observe" each kstat-level microstate a certain number of times, and
        // bump our counter of the oximeter state it maps to.
        let mut observed_states: BTreeMap<_, usize> = BTreeMap::new();
        for kstat_states in expected_states.values() {
            for (kstat_state, count) in kstat_states.iter() {
                let oximeter_state = kstat_microstate_to_state_name(
                    kstat_state,
                )
                .unwrap_or_else(|| {
                    panic!(
                        "kstat state '{}' did not map to an oximeter state, \
                        which it should have done. Did that state get \
                        mapped to a new oximeter-level state?",
                        kstat_state
                    )
                });
                *observed_states.entry(oximeter_state).or_default() += count;
            }
        }

        // Check that we've observed all the states correctly.
        assert_eq!(
            observed_states.len(),
            expected_states.len(),
            "Some oximeter-level states were not accounted for. \
            Did the set of oximeter states reported change?",
        );
        for (oximeter_state, count) in observed_states.iter() {
            let kstat_states = expected_states.get(oximeter_state).expect(
                "An unexpected oximeter state was produced. \
                    Did the set of kstat or oximeter microstates \
                    change?",
            );
            let expected_total: usize =
                kstat_states.iter().map(|(_, ct)| ct).sum();
            assert_eq!(
                *count, expected_total,
                "Some oximeter states were not accounted for",
            );
        }
    }
}
