// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::{self, BoxFuture};

use crate::migrate::Migrator;

/// General trait for emulated devices in the system.
///
/// As the VM goes through its lifecycle, the emulated devices which it contains
/// are so driven through those phases via various event functions (`start`,
/// `pause`, `resume`, etc).
///
/// NOTE: Calling any of these methods on a [Lifecycle] instance should be done
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
    ///
    /// WARNING: This function may only be called after pausing all of a VM's
    /// vCPUs. This allows components to mutate state (such as VM memory) in
    /// ways that should otherwise not be visible to a running vCPU.
    fn pause(&self) {}

    /// Directs this device to resume servicing the guest after pausing.
    ///
    /// WARNING: This function must be called before resuming any of a VM's
    /// vCPUs. This allows components to restore any world-state they preserved
    /// during their `pause` callouts before vCPUs get a chance to perceive it.
    ///
    /// NOTE: It is legal to call `reset` between pausing and resuming. If this
    /// occurs, the caller must ensure that all VM devices and CPUs will be
    /// reset and reinitialized before resuming any devices.
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

/// Indicator for tracking [Lifecycle] states.
///
/// As implementors of the [Lifecycle] trait are driven through various state
/// changes when called through its functions, the underlying resource may want
/// to keep track of which state it is in.  Device emulation may wish to
/// postpone certain setup tasks when undergoing state import during a
/// migration, for example.
///
/// The [Indicator] is meant as a convenience tool for such implementors to
/// simply track the current state by calling the appropriate methods
/// corresponding to their [Lifecycle] counterparts.
#[derive(Default)]
pub struct Indicator(AtomicUsize);
impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }
    /// Indicate that holder is has started
    ///
    /// To be called as part of the implementor's [Lifecycle::start()] method
    pub fn start(&self) {
        self.state_change(IndicatedState::Run);
    }
    /// Indicate that holder is has paused
    ///
    /// To be called as when [Lifecycle::pause()] has been called on the
    /// implementor _and_ that pause has completed (if there is async work
    /// required for such a state).
    pub fn pause(&self) {
        self.state_change(IndicatedState::Pause);
    }
    /// Indicate that holder is has resumed
    ///
    /// To be called as part of the implementor's [Lifecycle::resume()] method
    pub fn resume(&self) {
        self.state_change(IndicatedState::Run);
    }
    /// Indicate that holder is has halted
    ///
    /// To be called as part of the implementor's [Lifecycle::halt()] method
    pub fn halt(&self) {
        self.state_change(IndicatedState::Halt);
    }
    /// Get the currently indicated state for the holder
    pub fn state(&self) -> IndicatedState {
        IndicatedState::from_usize(self.0.load(Ordering::SeqCst))
    }

    fn state_change(&self, new: IndicatedState) {
        let old = self.0.swap(new as usize, Ordering::SeqCst);
        IndicatedState::assert_valid_transition(
            IndicatedState::from_usize(old),
            new,
        );
    }
}

/// Represents the current state held in [Indicator]
#[derive(Copy, Clone, Eq, PartialEq, strum::FromRepr, Debug)]
#[repr(usize)]
pub enum IndicatedState {
    /// Lifecycle entity has been created, but has not been
    /// (started)[Lifecycle::start()] yet.
    Init,
    /// Lifecycle entity is running
    Run,
    /// Lifecycle entity is paused
    Pause,
    /// Lifecycle entity has halted
    Halt,
}
impl IndicatedState {
    const fn valid_transition(old: Self, new: Self) -> bool {
        use IndicatedState::*;
        match (old, new) {
            (Init, Run) | (Init, Halt) => true,
            (Run, Pause) => true,
            (Pause, Run) | (Pause, Halt) => true,
            _ => false,
        }
    }
    fn from_usize(raw: usize) -> Self {
        IndicatedState::from_repr(raw)
            .expect("raw value {raw} should be valid IndicatedState")
    }
    fn assert_valid_transition(old: Self, new: Self) {
        assert!(
            Self::valid_transition(old, new),
            "transition from {old:?} to {new:?} is not valid"
        );
    }
}
