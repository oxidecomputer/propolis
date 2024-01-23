// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::inventory::Entity;
use crate::migrate::*;

use serde::{Deserialize, Serialize};

use slog::info;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A test device for simulating migration failures.
pub struct MigrationFailureDevice {
    log: slog::Logger,
    exports: AtomicUsize,
    fail_exports: usize,
    fail_imports: usize,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct MigrationFailurePayloadV1 {
    /// If set, an attempt to import this device should fail.
    fail_import: bool,
}

impl MigrationFailureDevice {
    pub const NAME: &'static str = "test-migration-failure";
    pub fn new(log: &slog::Logger) -> Self {
        Self {
            log: log.new(slog::o!("component" => "test", "dev" => Self::NAME)),
            exports: AtomicUsize::new(0),
            fail_exports: 0,
            fail_imports: 0,
        }
    }

    pub fn fail_exports(self, fail_exports: usize) -> Self {
        Self { fail_exports, ..self }
    }

    pub fn fail_imports(self, fail_exports: usize) -> Self {
        Self { fail_exports, ..self }
    }
}

impl Entity for MigrationFailureDevice {
    fn type_name(&self) -> &'static str {
        MigrationFailureDevice::NAME
    }
    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}

impl MigrateSingle for MigrationFailureDevice {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        let export_num = self.exports.fetch_add(1, Ordering::Relaxed);
        if export_num < self.fail_exports {
            info!(
                self.log,
                "failing export";
                "export_num" => %export_num,
                "fail_exports" => %self.fail_exports
            );
            return Err(MigrateStateError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "somebody set up us the bomb",
            )));
        }

        let fail_import = export_num < self.fail_imports;
        info!(
            self.log,
            "exporting device";
            "export_num" => %export_num,
            "will_fail_import" => %fail_import,
        );
        Ok(MigrationFailurePayloadV1 { fail_import }.into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let MigrationFailurePayloadV1 { fail_import } = offer.parse()?;
        if fail_import {
            info!(self.log, "failing import");
            return Err(MigrateStateError::ImportFailed(
                "you have no chance to survive, make your time".to_string(),
            ));
        }
        Ok(())
    }
}

impl Schema<'_> for MigrationFailurePayloadV1 {
    fn id() -> SchemaId {
        (MigrationFailureDevice::NAME, 1)
    }
}
