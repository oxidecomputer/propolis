// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::virtual_machine::VirtualMachine;
use chrono::Utc;
use oximeter::{
    types::{Cumulative, Sample},
    Metric, MetricsError, Producer,
};
use propolis::hw::qemu::pvpanic;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PvpanicProducer {
    /// The oximeter Target identifying this instance as the source of metric
    /// data.
    virtual_machine: VirtualMachine,

    /// Kernel panic counts for the relevant instance.
    host_handled_panics: PvPanicHostHandled,
    guest_handled_panics: PvPanicGuestHandled,

    pvpanic: Arc<pvpanic::QemuPvpanic>,
}

/// An Oximeter `Metric` that specifies the number of times an instance's guest
/// reported a guest-handled kernel panic using the QEMU `pvpanic` device.
//
// NOTE: We may want to collapse these into a single metric with a field
// indicating whether the guest or host handle bit was set, but it's not clear
// that these are truly mutually exclusive. It could also be done with two
// boolean fields, which would allow either to be set.
//
// The advantage of that is easier aggregation, since the counts are part of the
// same timeseries schema, only with different fields.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct PvPanicGuestHandled {
    /// The number of times this instance's guest handled a kernel panic.
    #[datum]
    pub count: Cumulative<u64>,
}

/// An Oximeter `Metric` that specifies the number of times an instance's guest
/// reported a host-handled kernel panic using the QEMU `pvpanic` device.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct PvPanicHostHandled {
    /// The number of times this instance's reported a host-handled kernel panic.
    #[datum]
    pub count: Cumulative<u64>,
}

impl PvpanicProducer {
    pub fn new(
        virtual_machine: VirtualMachine,
        pvpanic: Arc<pvpanic::QemuPvpanic>,
    ) -> Self {
        // Construct a single counter and copy, so the timeseries are aligned to
        // the same start time.
        let count = Default::default();
        PvpanicProducer {
            virtual_machine,
            host_handled_panics: PvPanicHostHandled { count },
            guest_handled_panics: PvPanicGuestHandled { count },
            pvpanic,
        }
    }
}

impl Producer for PvpanicProducer {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        let pvpanic::PanicCounts { guest_handled, host_handled } =
            self.pvpanic.panic_counts();

        self.host_handled_panics.datum_mut().set(host_handled as u64);
        self.guest_handled_panics.datum_mut().set(guest_handled as u64);

        // Provide both the same timestamp, to simplify alignment.
        let now = Utc::now();
        let data = [
            Sample::new_with_timestamp(
                now,
                &self.virtual_machine,
                &self.guest_handled_panics,
            )?,
            Sample::new_with_timestamp(
                now,
                &self.virtual_machine,
                &self.host_handled_panics,
            )?,
        ];

        Ok(Box::new(data.into_iter()))
    }
}
