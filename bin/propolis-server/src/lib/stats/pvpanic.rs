// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::InstanceUuid;
use oximeter::{
    types::{Cumulative, Sample},
    Metric, MetricsError, Producer,
};
use propolis::hw::qemu::pvpanic::QemuPvpanic;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct PvpanicProducer {
    /// The name to use as the Oximeter target, i.e. the identifier of the
    /// source of these metrics.
    stat_name: InstanceUuid,

    /// Kernel panic counts for the relevant instance.
    host_handled_panics: PvPanicHostHandled,
    guest_handled_panics: PvPanicGuestHandled,

    counts: Arc<QemuPvpanic>,
}

/// An Oximeter `Metric` that specifies the number of times an instance's guest
/// reported a guest-handled kernel panic using the QEMU `pvpanic` device.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct PvPanicGuestHandled {
    /// The number of times this instance's guest handled a kernel panic.
    #[datum]
    pub count: Cumulative<i64>,
}

/// An Oximeter `Metric` that specifies the number of times an instance's guest
/// reported a host-handled kernel panic using the QEMU `pvpanic` device.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct PvPanicHostHandled {
    /// The number of times this instance's reported a host-handled kernel panic.
    #[datum]
    pub count: Cumulative<i64>,
}

impl PvpanicProducer {
    pub fn new(id: Uuid, counts: Arc<QemuPvpanic>) -> Self {
        PvpanicProducer {
            stat_name: InstanceUuid { uuid: id },
            host_handled_panics: Default::default(),
            guest_handled_panics: Default::default(),
            counts,
        }
    }
}

impl Producer for PvpanicProducer {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        self.host_handled_panics
            .datum_mut()
            .set(self.counts.host_handled_count() as i64);
        self.guest_handled_panics
            .datum_mut()
            .set(self.counts.guest_handled_count() as i64);
        let data = vec![
            Sample::new(&self.stat_name, &self.guest_handled_panics)?,
            Sample::new(&self.stat_name, &self.host_handled_panics)?,
        ];

        Ok(Box::new(data.into_iter()))
    }
}
