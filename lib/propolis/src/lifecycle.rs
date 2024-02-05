// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use futures::future::{self, BoxFuture};

use crate::migrate::Migrator;

/// General trait for emulated devices in the system.
///
/// As the VM goes through its lifecycle, the emulated devices which it contains
/// are so driven through those phases via various event functions (`start`,
/// `pause`, `resume`, etc).
///
/// Note: Calling any of these methods on a [Lifecycle] instance should be done
/// from a context with access to a [Tokio runtime](tokio::runtime::Runtime):
///
/// - [Lifecycle::start]
/// - [Lifecycle::pause]
/// - [Lifecycle::resume]
/// - [Lifecycle::paused]
/// - [Lifecycle::reset]
/// - [Lifecycle::halt]
/// - [Lifecycle::migrate]
pub trait Lifecycle: Send + Sync + 'static {
    /// Unique name for devices for a given type
    ///
    /// Intended to be `const` once stabilized for trait functions.
    fn type_name(&self) -> &'static str;

    /// Return a future indicating when the device has finished pausing.
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }

    /// Called when an instance is about to begin running, just before any
    /// vCPU threads are started. After this returns, the callee device should
    /// be ready to do work on the guest's behalf.
    ///
    /// Note that this is only called the first time an instance begins running.
    /// If it reboots, the device will observe a paused -> reset -> resumed
    /// transition instead.
    fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Directs this device to pause. A paused device must stop producing work
    /// for other devices (and any associated backend(s)), but must accept (and
    /// hold onto) new work from other devices while in the paused state.
    ///
    /// The device is not required to finish pausing inline. Instead, its
    /// implementation of [`Lifecycle::paused`] should return a future that
    /// completes only when the device is paused.
    fn pause(&self) {}

    /// Directs this device to resume servicing the guest after pausing.
    ///
    /// N.B. It is legal to interpose a `reset` between a pause and resume.
    ///      If one occurs, the state driver ensures that the entire VM will be
    ///      reset and reinitialized before any devices are resumed.
    fn resume(&self) {}

    /// Directs this device to reset itself to the state it would have on a cold
    /// start.
    ///
    /// N.B. The state driver ensures this is called only on paused devices.
    ///      It also ensures that the entire VM will be reset and reinitialized
    ///      before resuming any devices.
    fn reset(&self) {}

    /// Indicates that the device's instance is stopping and will soon be
    /// discarded.
    ///
    /// N.B. The state driver ensures this is called only on paused devices.
    fn halt(&self) {}

    /// Return the Migrator object that will be used to export/import
    /// this device's state.
    ///
    /// By default, we return a simple impl that assumes the device
    /// has no state that needs to be exported/imported but still wants
    /// to opt into being migratable. For more complex cases, a device
    /// may implement the `Migrate` trait along with its export/import
    /// methods. A device which shouldn't be migrated should instead
    /// override this method and explicity return [`Migrator::NonMigratable`].
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }
}
