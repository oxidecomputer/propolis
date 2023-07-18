// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::Result;
use phd_framework::artifacts::ArtifactStore;
use tracing::instrument;

use crate::TestContext;

/// A wrapper containing the objects needed to run the executor's test fixtures.
pub struct TestFixtures<'a> {
    artifact_store: &'a ArtifactStore,
    test_context: &'a TestContext<'a>,
}

impl<'a> TestFixtures<'a> {
    /// Creates a new set of test fixtures using the supplied command-line
    /// parameters and artifact store.
    pub fn new(
        artifact_store: &'a ArtifactStore,
        test_context: &'a TestContext,
    ) -> Result<Self> {
        Ok(Self { artifact_store, test_context })
    }

    /// Calls fixture routines that need to run before any tests run.
    #[instrument(skip_all)]
    pub fn execution_setup(&mut self) -> Result<()> {
        self.artifact_store.check_local_copies()
    }

    /// Calls fixture routines that need to run after all tests run.
    ///
    /// Unless the runner panics, or a test panics in a way that can't be caught
    /// during unwinding, this cleanup fixture will run even if a test run is
    /// interrupted.
    #[instrument(skip_all)]
    pub fn execution_cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    /// Calls fixture routines that run before each test case is invoked.
    #[instrument(skip_all)]
    pub fn test_setup(&mut self) -> Result<()> {
        Ok(())
    }

    /// Calls fixture routines that run after each test case is invoked.
    ///
    /// Unless the runner panics, or a test panics in a way that can't be caught
    /// during unwinding, this cleanup fixture will run whenever the
    /// corresponding setup fixture has run.
    #[instrument(skip_all)]
    pub fn test_cleanup(&mut self) -> Result<()> {
        self.test_context.vm_factory.reset();
        Ok(())
    }
}
