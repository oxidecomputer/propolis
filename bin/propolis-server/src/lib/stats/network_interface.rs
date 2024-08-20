// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

//! Types and functions for tracking statistics about an instance's network
//! interfaces.

// Propolis is built in a variety of configurations, including checks and tests
// run on non-illumos machines where kstats are meaningless. This is a big
// hammer, but a large number of the values in this module are not referenced in
// those configurations, and so this is more straightfoward than littering the
// code with cfg directives.
#![cfg_attr(any(test, not(target_os = "illumos")), allow(dead_code))]

use chrono::{DateTime, Utc};
use oximeter::{types::Cumulative, FieldType, FieldValue, Sample, Target};

use super::kstat_types::{
    hrtime_to_utc, ConvertNamedData, Data, Error, Kstat, KstatList,
    KstatTarget, Named,
};
use crate::vm::NetworkInterfaceIds;

// NOTE: TOML definitions of timeseries are centralized in Omicron, so this file
// lives in that repo, at
// `./omicron/oximeter/oximeter/schema/instance-network-interface.toml`.
oximeter::use_timeseries!("instance-network-interface.toml");
use self::instance_network_interface::{
    BytesReceived, BytesSent, ErrorsReceived, ErrorsSent,
    InstanceNetworkInterface, PacketsDropped, PacketsReceived, PacketsSent,
};

const KSTAT_RX_BYTES: &str = "rx_bytes";
const KSTAT_TX_BYTES: &str = "tx_bytes";
const KSTAT_RX_PACKETS: &str = "rx_packets";
const KSTAT_TX_PACKETS: &str = "tx_packets";
const KSTAT_RX_DROPS: &str = "rx_drops";
const KSTAT_RX_ERRORS: &str = "rx_errors";
const KSTAT_TX_ERRORS: &str = "tx_errors";

/// The names of the kstat fields that represent the instance network interface metrics
/// we are interested in tracking.
const KSTAT_FIELDS: &[&str] = &[
    KSTAT_RX_BYTES,
    KSTAT_TX_BYTES,
    KSTAT_RX_PACKETS,
    KSTAT_TX_PACKETS,
    KSTAT_RX_DROPS,
    KSTAT_RX_ERRORS,
    KSTAT_TX_ERRORS,
];

/// The name of the kstat module that contains the instance network interface
/// metrics.
const KSTAT_MODULE_NAME: &str = "viona";

/// The name of the kstat that contains the instance network interface metrics.
const KSTAT_NAME: &str = "viona_stat";

/// Helper function to extract the same kstat metrics from all link targets.
fn extract_nic_kstats(
    target: &InstanceNetworkInterface,
    named_data: &Named,
    creation_time: DateTime<Utc>,
    snapshot_time: DateTime<Utc>,
) -> Option<Result<Sample, Error>> {
    let Named { name, value } = named_data;
    if *name == KSTAT_RX_BYTES {
        Some(value.as_u64().and_then(|x| {
            let metric = BytesReceived {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_TX_BYTES {
        Some(value.as_u64().and_then(|x| {
            let metric = BytesSent {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_RX_PACKETS {
        Some(value.as_u64().and_then(|x| {
            let metric = PacketsReceived {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_TX_PACKETS {
        Some(value.as_u64().and_then(|x| {
            let metric = PacketsSent {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_RX_DROPS {
        Some(value.as_u64().and_then(|x| {
            let metric = PacketsDropped {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_RX_ERRORS {
        Some(value.as_u64().and_then(|x| {
            let metric = ErrorsReceived {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else if *name == KSTAT_TX_ERRORS {
        Some(value.as_u64().and_then(|x| {
            let metric = ErrorsSent {
                datum: Cumulative::with_start_time(creation_time, x),
            };
            Sample::new_with_timestamp(snapshot_time, target, &metric)
                .map_err(Error::Sample)
        }))
    } else {
        None
    }
}

/// A wrapper around the `oximeter::Target` representing all instance network interfaces.
#[derive(Clone, Debug)]
pub(crate) struct InstanceNetworkInterfaces {
    /// The `oximeter::Target` itself, storing the target fields for the
    /// timeseries.
    ///
    /// **NOTE**: While this struct represents multiple instance network interfaces,
    /// they all share the same target fields.
    ///
    /// We default `interface_id` by generating a `uuid::Uuid::nil()` on first
    /// creation, before creating multiple targets in the `to_samples` method.
    pub(crate) target: InstanceNetworkInterface,

    /// A tuple-mapping of the interface UUIDs to the kstat instance IDs.
    pub(crate) interface_ids: NetworkInterfaceIds,
}

impl InstanceNetworkInterfaces {
    /// Create a new instance network interface metrics target from the given
    /// instance properties and add the interface_ids to match and gather
    /// metrics from.
    #[cfg(all(not(test), target_os = "illumos"))]
    pub(crate) fn new(
        properties: &propolis_api_types::InstanceProperties,
        interface_ids: NetworkInterfaceIds,
    ) -> Self {
        Self {
            target: InstanceNetworkInterface {
                // Default `interface_id` to a new UUID, as we will create
                // multiple targets in the `to_samples` method and override
                // this.
                interface_id: uuid::Uuid::nil(),
                instance_id: properties.id,
                project_id: properties.metadata.project_id,
                silo_id: properties.metadata.silo_id,
            },
            interface_ids: interface_ids.to_vec(),
        }
    }
}

impl KstatTarget for InstanceNetworkInterfaces {
    fn interested(&self, kstat: &Kstat<'_>) -> bool {
        kstat.ks_module == KSTAT_MODULE_NAME
            && kstat.ks_name == KSTAT_NAME
            && self.interface_ids.iter().any(|(_id, device_instance_id)| {
                kstat.ks_instance as u32 == *device_instance_id
            })
    }

    fn to_samples(
        &self,
        kstats: KstatList<'_, '_>,
    ) -> Result<Vec<Sample>, Error> {
        let kstats_for_nics =
            kstats.iter().filter_map(|(creation_time, kstat, data)| {
                self.interface_ids.iter().find_map(
                    |(id, device_instance_id)| {
                        if kstat.ks_instance as u32 == *device_instance_id {
                            let target = InstanceNetworkInterface {
                                interface_id: *id,
                                instance_id: self.target.instance_id,
                                project_id: self.target.project_id,
                                silo_id: self.target.silo_id,
                            };
                            Some((*creation_time, kstat, data, target))
                        } else {
                            None
                        }
                    },
                )
            });

        // Capacity is determined by the number of interfaces times the number
        // of kstat fields we track.
        let mut out =
            Vec::with_capacity(self.interface_ids.len() * KSTAT_FIELDS.len());
        for (creation_time, kstat, data, target) in kstats_for_nics {
            let snapshot_time = hrtime_to_utc(kstat.ks_snaptime)?;
            if let Data::Named(named) = data {
                named
                    .iter()
                    .filter_map(|nd| {
                        extract_nic_kstats(
                            &target,
                            nd,
                            creation_time,
                            snapshot_time,
                        )
                        .and_then(|opt_result| opt_result.ok())
                    })
                    .for_each(|sample| out.push(sample));
            }
        }

        Ok(out)
    }
}

// Implement the `oximeter::Target` trait for `InstanceNetworkInterfaces` using
// the single `InstanceNetworkInterface` target as it represents all the same fields.
impl Target for InstanceNetworkInterfaces {
    fn name(&self) -> &'static str {
        self.target.name()
    }
    fn field_names(&self) -> &'static [&'static str] {
        self.target.field_names()
    }

    fn field_types(&self) -> Vec<FieldType> {
        self.target.field_types()
    }

    fn field_values(&self) -> Vec<FieldValue> {
        self.target.field_values()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use uuid::Uuid;

    use super::*;
    use crate::stats::kstat_types::NamedData;

    fn test_network_interface() -> InstanceNetworkInterface {
        const INTERFACE_ID: Uuid =
            uuid::uuid!("f4b3b3b3-3b3b-3b3b-3b3b-3b3b3b3b3b3b");
        const INSTANCE_ID: Uuid =
            uuid::uuid!("96d6ec78-543a-4188-830e-37e2a0eeff16");
        const PROJECT_ID: Uuid =
            uuid::uuid!("7b61df02-0794-4b37-93bc-89f03c7289ca");
        const SILO_ID: Uuid =
            uuid::uuid!("6a4bd4b6-e9aa-44d1-b616-399d48baa173");

        InstanceNetworkInterface {
            interface_id: INTERFACE_ID,
            instance_id: INSTANCE_ID,
            project_id: PROJECT_ID,
            silo_id: SILO_ID,
        }
    }

    #[test]
    fn test_kstat_interested() {
        let target = InstanceNetworkInterfaces {
            target: test_network_interface(),
            interface_ids: vec![(Uuid::new_v4(), 2), (Uuid::new_v4(), 3)],
        };

        let ks_interested2 = Kstat {
            ks_module: KSTAT_MODULE_NAME,
            ks_instance: 2,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };

        assert!(target.interested(&ks_interested2));

        let ks_interested3 = Kstat {
            ks_module: KSTAT_MODULE_NAME,
            ks_instance: 3,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };

        assert!(target.interested(&ks_interested3));

        let ks_not_interested_module = Kstat {
            ks_module: "not-viona",
            ks_instance: 2,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };
        assert!(!target.interested(&ks_not_interested_module));

        let ks_not_interested_instance = Kstat {
            ks_module: KSTAT_MODULE_NAME,
            ks_instance: 4,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };
        assert!(!target.interested(&ks_not_interested_instance));
    }

    #[test]
    fn test_kstat_to_samples() {
        let target = InstanceNetworkInterfaces {
            target: test_network_interface(),
            interface_ids: vec![(Uuid::new_v4(), 2), (Uuid::new_v4(), 3)],
        };

        let kstat2 = Kstat {
            ks_module: KSTAT_MODULE_NAME,
            ks_instance: 2,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };

        let kstat3 = Kstat {
            ks_module: KSTAT_MODULE_NAME,
            ks_instance: 3,
            ks_snaptime: 0,
            ks_name: KSTAT_NAME,
        };

        let bytes_received =
            Named { name: KSTAT_RX_BYTES, value: NamedData::UInt64(100) };
        let bytes_sent =
            Named { name: KSTAT_TX_BYTES, value: NamedData::UInt64(200) };
        let packets_received =
            Named { name: KSTAT_RX_PACKETS, value: NamedData::UInt64(4) };
        let packets_sent =
            Named { name: KSTAT_TX_PACKETS, value: NamedData::UInt64(4) };
        let packets_dropped =
            Named { name: KSTAT_RX_DROPS, value: NamedData::UInt64(0) };
        let errors_received =
            Named { name: KSTAT_RX_ERRORS, value: NamedData::UInt64(0) };
        let errors_sent =
            Named { name: KSTAT_TX_ERRORS, value: NamedData::UInt64(0) };

        let data = Data::Named(vec![
            bytes_received,
            bytes_sent,
            packets_received,
            packets_sent,
            packets_dropped,
            errors_received,
            errors_sent,
        ]);

        let kstat_list = vec![
            (Utc::now(), kstat2, data.clone()),
            (Utc::now(), kstat3, data),
        ];
        let samples = target.to_samples(kstat_list.as_slice()).unwrap();
        assert_eq!(samples.len(), 2 * KSTAT_FIELDS.len());

        let mut interface_uuids = HashSet::new();
        for sample in samples {
            assert_eq!(sample.target_name(), "instance_network_interface");
            for field in sample.fields() {
                assert!(target.field_names().contains(&field.name.as_str()));
                if field.name == "interface_id" {
                    interface_uuids.insert(field.value);
                }
            }
        }
        // We should have two unique interface UUIDs.
        assert_eq!(interface_uuids.len(), 2);
    }
}
