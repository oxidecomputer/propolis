// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::virtual_machine::VirtualMachine;
use chrono::Utc;
use oximeter::{types::Sample, Metric, MetricsError, Producer};
use propolis::hw::qemu::pvpanic;
use std::sync::Arc;

// NOTE: TOML definitions of timeseries are centralized in Omicron, so this file
// lives in that repo, at
// `./omicron/oximeter/oximeter/schema/virtual-machine.toml`.
oximeter::use_timeseries!("virtual-machine.toml");
use self::virtual_machine::{PvPanicGuestHandled, PvPanicHostHandled};

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

impl PvpanicProducer {
    pub fn new(
        virtual_machine: VirtualMachine,
        pvpanic: Arc<pvpanic::QemuPvpanic>,
    ) -> Self {
        // Construct a single counter and copy, so the timeseries are aligned to
        // the same start time.
        let datum = Default::default();
        PvpanicProducer {
            virtual_machine,
            host_handled_panics: PvPanicHostHandled { datum },
            guest_handled_panics: PvPanicGuestHandled { datum },
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
