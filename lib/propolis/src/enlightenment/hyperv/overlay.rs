// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for hypervisor overlay pages.
//!
//! # Overview
//!
//! An _overlay page_ is a page of guest memory that "covers" a physical page in
//! the guest's normal physical address space. The physical analogue might be a
//! computer with a special one-page memory in addition to its regular RAM whose
//! memory controller can be asked to redirect accesses to one page's worth of
//! physical addresses to the auxiliary memory. The physical page at this
//! address still exists in main memory, but it can't be addressed while the
//! memory controller is applying the overlay.
//!
//! The virtualized case is similar, but there is only one physical memory (the
//! host's memory), and the host plays the role of the memory controller,
//! changing the guest's "view" of a particular GPA as overlays are applied and
//! removed. One way to do this is to change the VM's nested page table entries
//! (i.e., the translations from GPAs to HPAs) to redirect GPA accesses to a
//! different host physical page. bhyve's memory management infrastructure
//! doesn't currently support this, so this module emulates overlays in software
//! by saving and restoring the contents of the various "layers" that exist at a
//! particular GPA and making sure the active layer is present in the host
//! physical page to which bhyve has mapped the relevant GPA.
//!
//! In general, the Hyper-V stack establishes overlays in response to a guest's
//! request to place a particular overlay at a particular GPA. For example, the
//! guest writes to the `HV_X64_MSR_HYPERCALL` MSR to provide the PFN of a page
//! that the guest would like to see overlaid with a hypercall instruction page.
//! This implies that overlays don't change while the guest's CPUs are paused;
//! this has useful implications for the lifetimes of the structures in this
//! module, which are described further below.
//!
//! TLFS section 5.2.1 describes the semantics of overlay pages in more detail.
//! Importantly, the TLFS specifies that when multiple overlays exist for a
//! single GPA, their logical ordering is implementation-defined (and not, for
//! example, based on the order in which the overlays were requested).
//!
//! The TLFS doesn't explicitly specify the sizes of its overlay pages, but all
//! the overlays this module cares about are 4 KiB, because the guest specifies
//! their locations using 52-bit PFNs.
//!
//! # This module
//!
//! ## Public interface
//!
//! This module's public interface is built around two main types:
//!
//! - The [`OverlayManager`] tracks the set of active overlays in the system and
//!   allows users to create new overlays.
//! - An [`OverlayPage`] represents a single overlay and provides interfaces
//!   to move it or remove it (the latter by dropping the page). Pages hold a
//!   reference (in fact a weak reference; see below) to their managers so that
//!   they can unregister themselves when they are dropped.
//!
//! Each [`super::HyperV`] instance holds a strong reference to an
//! `OverlayManager` and creates new overlays in response to guest activity
//! (e.g., a vCPU writing a requested overlay GPA to a Hyper-V MSR). The
//! `HyperV` instance owns all of the `OverlayPage`s it creates this way and
//! drops them or calls [`OverlayPage::move_to`] on them in response to further
//! guest activity.
//!
//! ## Dealing with VM shutdown
//!
//! When a VM is active, dropping an active `OverlayPage` should always remove
//! its contents from guest memory and replace them either with another overlay
//! page's contents or the original guest memory contents. Failing to do this
//! means that guest memory has been corrupted, so it's desirable to panic if
//! dropping an overlay page ever fails to remove it from the manager.
//!
//! The catch is that once a VM has halted, its overlay manager is no longer
//! guaranteed to be able to access guest memory (since its [`MemAccessor`] may
//! have been disconnected). To handle this while still being able to panic on
//! failure:
//!
//! - `OverlayPage`s hold [`Weak`] references to their `OverlayManager`s.
//! - [`OverlayPage::drop`] only tries to remove an overlay if it can upgrade
//!   its manager reference.
//! - When a manager's owner receives a [`Lifecycle::halt`] callout, it releases
//!   all its strong references to its `OverlayManager`. (This is generally
//!   possible because a halting VM's vCPUs are permanently quiesced and so
//!   can't write any more data that would create or move another overlay.)
//!
//! This pattern allows a `HyperV` instance to "run down" its manager by
//! simply dropping all references to it. After that, it can drop its
//! `OverlayPage`s at any time without fear of them trying and failing to access
//! guest memory.
//!
//! [`Lifecycle::halt`]: crate::lifecycle::Lifecycle::halt

use std::{
    collections::{btree_map::Entry, BTreeMap},
    sync::{Arc, Mutex, Weak},
};

use thiserror::Error;

use crate::{
    accessors::MemAccessor, common::PAGE_SIZE, migrate::MigrateStateError,
    vmm::Pfn,
};

use self::pfn::MappedPfn;

/// An error that can be returned from an overlay page operation.
#[derive(Debug, Error)]
pub enum OverlayError {
    /// The guest memory context can't be accessed right now. Generally this
    /// means that the caller is trying to create an overlay too early (i.e.,
    /// before the overlay manager is attached to the memory hierarchy) or too
    /// late (i.e., after the VM has shut down).
    #[error("guest memory is currently inaccessible")]
    GuestMemoryInaccessible,

    /// The supplied physical address is either out of the guest's physical
    /// address range or is in a region to which the guest lacks read/write
    /// access.
    #[error("overlay target GPA {0:#x} is inaccessible")]
    AddressInaccessible(u64),

    /// An overlay of the supplied kind already exists for the supplied PFN.
    #[error("overlay target PFN {0:#x} already has an overlay of kind {1:?}")]
    KindInUse(u64, OverlayKind),

    /// The requested operation requires an existing overlay at the supplied
    /// PFN, but that PFN has no active overlays.
    #[error("no overlays registered for PFN {0:#x}")]
    NoOverlaysActive(u64),

    /// The supplied PFN has overlays, but not of the kind requested by the
    /// caller.
    #[error("PFN {0:#x} has no overlay of kind {1:?}")]
    NoOverlayOfKind(u64, OverlayKind),

    #[error("imported PFN {0:#x} is too large to be a 4K page number")]
    ImportedPfnTooLarge(u64),

    /// An overlay manager import payload contained overlay page contents
    /// that are not page-sized.
    #[error("imported contents for pfn {pfn:#x} have invalid length {len}")]
    ImportedContentsNotPageSized { pfn: u64, len: usize },

    /// An overlay manager import payload contained an overlay set where a
    /// single kind of overlay was both active and pending.
    #[error("imported overlay set for pfn {0:#x} has duplicate kind {1:?}")]
    ImportedSetDuplicateKind(u64, OverlayKind),
}

impl From<OverlayError> for MigrateStateError {
    fn from(value: OverlayError) -> Self {
        MigrateStateError::ImportFailed(format!(
            "error importing overlay state: {value}"
        ))
    }
}

/// The contents of a 4 KiB page. These are boxed so that this type can be
/// embedded in a struct that's put into a contiguous collection without putting
/// entire pages between collection members.
pub(super) struct OverlayContents(pub(super) Box<[u8; PAGE_SIZE]>);

impl Default for OverlayContents {
    fn default() -> Self {
        Self(Box::new([0u8; PAGE_SIZE]))
    }
}

impl TryFrom<Vec<u8>> for OverlayContents {
    type Error = usize;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let len = value.len();
        Ok(Self(Box::new(value.try_into().map_err(|_| len)?)))
    }
}

impl std::fmt::Debug for OverlayContents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OverlayContents").field(&"<page redacted>").finish()
    }
}

/// A kind of overlay page.
#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum OverlayKind {
    Hypercall,

    #[cfg(test)]
    Test(u8),
}

/// A registered overlay page, held by a user of this module. The overlay is
/// removed when this page is dropped.
pub(super) struct OverlayPage {
    /// A back reference to this page's manager. This is `Weak` so that pages
    /// can detect when the manager's Hyper-V stack has halted and can thereby
    /// avoid assuming that page removals should always succeed. See the module
    /// docs for details.
    manager: Weak<OverlayManager>,

    /// The kind of overlay this is.
    kind: OverlayKind,

    /// The PFN at which this overlay is currently applied.
    pfn: Pfn,
}

// Manually implemented to avoid including the `Weak<OverlayManager>`.
impl std::fmt::Debug for OverlayPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { kind, pfn, manager: _ } = self;
        f.debug_struct("OverlayPage")
            .field("kind", &kind)
            .field("pfn", &pfn)
            .finish()
    }
}

impl OverlayPage {
    pub(super) fn kind(&self) -> OverlayKind {
        self.kind
    }

    pub(super) fn pfn(&self) -> Pfn {
        self.pfn
    }

    /// Moves this overlay to a new PFN.
    ///
    /// # Panics
    ///
    /// Panics if called when there are no more strong references to the page's
    /// manager (which implies that the VM has shut down and that guest memory
    /// may not be accessible).
    pub(super) fn move_to(&mut self, new_pfn: Pfn) -> Result<(), OverlayError> {
        let manager = self
            .manager
            .upgrade()
            .expect("can only move overlay pages with an active manager");

        manager.move_overlay(self.pfn, new_pfn, self.kind)?;
        self.pfn = new_pfn;
        Ok(())
    }
}

impl Drop for OverlayPage {
    fn drop(&mut self) {
        // This overlay is already registered and has a valid PFN, so the only
        // reason the manager could fail to remove it is if it failed to access
        // guest memory entirely. The parent enlightenment stack prevents this
        // by holding a strong reference to the manager until the VM is halted.
        //
        // Once the VM *is* halted, no one will access guest memory again, so
        // it's OK to return without attempting to restore the page's previous
        // contents.
        let Some(manager) = self.manager.upgrade() else {
            return;
        };

        manager
            .remove_overlay(self.pfn, self.kind)
            .expect("active overlay pages can always be removed");
    }
}

/// A set of overlays that apply to a particular PFN. Only one overlay of a
/// given type may be applied to a particular PFN at a particular time.
#[derive(Debug)]
struct OverlaySet {
    /// The contents of guest memory at this set's PFN at the time an overlay
    /// first became active here.
    original_contents: OverlayContents,

    /// The kind of overlay that is currently active for this set's PFN.
    active: OverlayKind,

    /// The overlays that are waiting to be applied to this PFN.
    pending: BTreeMap<OverlayKind, OverlayContents>,
}

impl OverlaySet {
    /// Returns `true` if this set contains an overlay of the supplied kind.
    fn contains_kind(&self, kind: OverlayKind) -> bool {
        self.active == kind || self.pending.contains_key(&kind)
    }
}

/// Specifies whether a call to remove an overlay should return the overlay's
/// contents to the caller.
///
/// Callers who don't intend to use the contents of a removed overlay should use
/// `RemoveReturnContents::No` to avoid allocating and copying if they end up
/// removing an active overlay.
#[derive(PartialEq, Eq)]
enum RemoveReturnContents {
    Yes,
    No,
}

/// Synchronized overlay manager state.
#[derive(Default)]
struct ManagerInner {
    /// Tracks the set of PFNs that have active and/or pending overlays.
    overlays: BTreeMap<Pfn, OverlaySet>,
}

impl ManagerInner {
    /// Registers a new overlay at the supplied PFN.
    fn add_overlay(
        &mut self,
        pfn: Pfn,
        kind: OverlayKind,
        contents: OverlayContents,
        acc_mem: &MemAccessor,
    ) -> Result<(), OverlayError> {
        let memctx =
            acc_mem.access().ok_or(OverlayError::GuestMemoryInaccessible)?;

        let mut mapped = MappedPfn::new(pfn, &memctx)?;
        self.add_overlay_using_mapping(&mut mapped, kind, contents)
    }

    /// Applies a new overlay to guest memory using the supplied mapping.
    fn add_overlay_using_mapping(
        &mut self,
        mapped_pfn: &mut MappedPfn,
        kind: OverlayKind,
        contents: OverlayContents,
    ) -> Result<(), OverlayError> {
        let pfn = mapped_pfn.pfn();
        match self.overlays.entry(pfn) {
            Entry::Vacant(e) => {
                let mut original_contents = OverlayContents::default();
                mapped_pfn.read_page(&mut original_contents);
                mapped_pfn.write_page(&contents);
                e.insert(OverlaySet {
                    original_contents,
                    active: kind,
                    pending: Default::default(),
                });
            }
            Entry::Occupied(e) => {
                let set = e.into_mut();
                if set.contains_kind(kind) {
                    return Err(OverlayError::KindInUse(pfn.into(), kind));
                }

                set.pending.insert(kind, contents);
            }
        }

        Ok(())
    }

    /// Removes an existing overlay of the supplied kind from the supplied PFN.
    ///
    /// If `return_contents` is [`RemoveReturnContents::Yes`], then on success,
    /// this function returns an `Ok(Some)` containing the removed overlay's
    /// previous contents; otherwise, it returns `Ok(None)` on success.
    fn remove_overlay(
        &mut self,
        pfn: Pfn,
        kind: OverlayKind,
        acc_mem: &MemAccessor,
        return_contents: RemoveReturnContents,
    ) -> Result<Option<OverlayContents>, OverlayError> {
        let Entry::Occupied(mut set) = self.overlays.entry(pfn) else {
            return Err(OverlayError::NoOverlaysActive(pfn.into()));
        };

        let old_contents;
        if set.get().active == kind {
            let memctx = acc_mem
                .access()
                .ok_or(OverlayError::GuestMemoryInaccessible)?;

            let mut mapped = MappedPfn::new(pfn, &memctx)
                .expect("active overlay PFNs can be mapped");

            old_contents = if return_contents == RemoveReturnContents::Yes {
                let mut contents = OverlayContents::default();
                mapped.read_page(&mut contents);
                Some(contents)
            } else {
                None
            };

            // If there are no pending overlays left for this PFN, restore the
            // original contents of guest memory. After this the entire overlay
            // set is defunct and can be removed from the PFN map.
            if set.get().pending.is_empty() {
                mapped.write_page(&set.get().original_contents);
                set.remove_entry();
            } else {
                let set = set.get_mut();

                // TLFS section 5.2.1 specifies that when there are multiple
                // overlays for a given page, the order in which they are
                // applied is implementation-defined, so any method of choosing
                // a new overlay from the pending set is sufficient.
                //
                // Unwrapping is safe here because the set was already checked
                // for emptiness.
                let (kind, contents) = set.pending.pop_first().unwrap();
                mapped.write_page(&contents);
                set.active = kind;
            }
        } else {
            // This overlay kind isn't active for this page. If it's pending,
            // it's sufficient just to remove its entry from the pending map,
            // since it has had no effect on guest memory.
            //
            // If there's no pending overlay of this kind, the caller goofed.
            let removed = set
                .get_mut()
                .pending
                .remove(&kind)
                .ok_or(OverlayError::NoOverlayOfKind(pfn.into(), kind))?;

            old_contents = if return_contents == RemoveReturnContents::Yes {
                Some(removed)
            } else {
                None
            };
        }

        Ok(old_contents)
    }

    /// Moves the overlay of the supplied `kind` from `from_pfn` to `to_pfn`.
    fn move_overlay(
        &mut self,
        from_pfn: Pfn,
        to_pfn: Pfn,
        kind: OverlayKind,
        acc_mem: &MemAccessor,
    ) -> Result<(), OverlayError> {
        if from_pfn == to_pfn {
            return Ok(());
        }

        let memctx =
            acc_mem.access().ok_or(OverlayError::GuestMemoryInaccessible)?;

        // Moving an overlay consists of applying it to its new location and
        // then removing it from its old one. This is only legal if the target
        // PFN is valid and there is no other overlay of this kind associated
        // with that PFN.
        //
        // Checking these conditions up front allows this function to attempt to
        // remove the existing overlay (checking for errors) and then assert
        // that the overlay can be applied in its new position. This is simpler
        // than removing the old overlay, failing to add the new one, and then
        // having to remember how to add the old overlay back in its previous
        // position.
        let mut to_mapping = MappedPfn::new(to_pfn, &memctx)?;
        if let Some(to_set) = self.overlays.get(&to_pfn) {
            if to_set.contains_kind(kind) {
                return Err(OverlayError::KindInUse(to_pfn.into(), kind));
            }
        }

        // Removing the old overlay might fail if it doesn't exist in
        // `from_pfn`, but if it succeeds it will always produce the previous
        // page contents.
        let contents = self
            .remove_overlay(from_pfn, kind, acc_mem, RemoveReturnContents::Yes)?
            .expect("asked for old contents");

        // Adding a new overlay with a valid mapping can only fail if the PFN
        // already has an overlay of the selected kind, and that was checked
        // above.
        self.add_overlay_using_mapping(&mut to_mapping, kind, contents)
            .expect("already checked new PFN for an overlay of this kind");

        Ok(())
    }

    fn export(&self) -> migrate::HyperVOverlaysV1 {
        let map = self
            .overlays
            .iter()
            .map(|(pfn, set)| {
                let set = migrate::OverlaySetV1 {
                    original_contents: set.original_contents.0.to_vec(),
                    active: set.active.into(),
                    pending: set
                        .pending
                        .iter()
                        .map(|(kind, contents)| {
                            (
                                migrate::MigratedOverlayKind::from(*kind),
                                contents.0.to_vec(),
                            )
                        })
                        .collect(),
                };

                (u64::from(*pfn), set)
            })
            .collect();

        migrate::HyperVOverlaysV1 { overlays: map }
    }

    fn import(
        &mut self,
        manager: &Arc<OverlayManager>,
        payload: migrate::HyperVOverlaysV1,
    ) -> Result<Vec<OverlayPage>, OverlayError> {
        // Take each serialized (PFN, overlay set) tuple and convert it back
        // into an `OverlaySet` that can be inserted into the map. Because the
        // input payload came over the wire, it is not strictly guaranteed that
        // it came from a Propolis that upheld all of the overlay set
        // invariants, so these need to be rechecked and an error returned if
        // one is violated. These include:
        //
        // - All overlay page contents must in fact be 4K in size.
        // - All PFNs must be valid 52-bit page numbers for 4K pages.
        // - Each overlay kind must appear at most once in each overlay set.
        self.overlays = payload
            .overlays
            .into_iter()
            .map(|(pfn, set)| -> Result<(Pfn, OverlaySet), OverlayError> {
                let original_contents = OverlayContents::try_from(
                    set.original_contents,
                )
                .map_err(|len| {
                    OverlayError::ImportedContentsNotPageSized { pfn, len }
                })?;

                let pfn = Pfn::new(pfn)
                    .ok_or(OverlayError::ImportedPfnTooLarge(pfn))?;

                let pending = set
                    .pending
                    .into_iter()
                    .map(|(kind, contents)| -> Result<_, OverlayError> {
                        if kind == set.active {
                            return Err(OverlayError::KindInUse(
                                pfn.into(),
                                kind.into(),
                            ));
                        }

                        let contents = OverlayContents::try_from(contents)
                            .map_err(|len| {
                                OverlayError::ImportedContentsNotPageSized {
                                    pfn: pfn.into(),
                                    len,
                                }
                            })?;

                        Ok((OverlayKind::from(kind), contents))
                    })
                    .collect::<Result<BTreeMap<_, _>, OverlayError>>()?;

                Ok((
                    pfn,
                    OverlaySet {
                        original_contents,
                        active: set.active.into(),
                        pending,
                    },
                ))
            })
            .collect::<Result<BTreeMap<Pfn, OverlaySet>, OverlayError>>()?;

        // Now that the tracking tables have been reconstructed, create a set of
        // overlay page descriptors to return to the caller. The caller will
        // audit these to make sure they align with other enlightenment state.
        let pages: Vec<OverlayPage> = self
            .overlays
            .iter()
            .map(|(pfn, set)| {
                let mut entries = vec![OverlayPage {
                    manager: Arc::downgrade(manager),
                    kind: set.active,
                    pfn: *pfn,
                }];

                entries.extend(set.pending.keys().map(|kind| OverlayPage {
                    manager: Arc::downgrade(manager),
                    kind: *kind,
                    pfn: *pfn,
                }));

                entries
            })
            .reduce(|mut acc, mut e| {
                acc.append(&mut e);
                acc
            })
            .unwrap_or_default();

        Ok(pages)
    }
}

/// An overlay tracker that adds, removes, and moves overlays.
///
/// # Usage requirements
///
/// `HyperV` instances that own an overlay manager are expected to do the
/// following:
///
/// - After calling [`OverlayManager::new`], the manager's owner must call
///   [`OverlayManager::attach`] to attach the manager's memory accessor to a
///   VM's memory hierarchy before it can add any overlays.
/// - When the manager's owner halts, it must drop all strong references to its
///   overlay manager before dropping any remaining [`OverlayPage`]s.
///
/// See the module docs for more information.
pub(super) struct OverlayManager {
    inner: Mutex<ManagerInner>,
    acc_mem: MemAccessor,
}

impl Default for OverlayManager {
    fn default() -> Self {
        let acc_mem = MemAccessor::new_orphan();
        Self { inner: Mutex::new(ManagerInner::default()), acc_mem }
    }
}

impl OverlayManager {
    /// Creates a new overlay manager.
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Attaches this overlay manager to the supplied memory accessor hierarchy.
    pub(super) fn attach(&self, parent_mem: &MemAccessor) {
        parent_mem
            .adopt(&self.acc_mem, Some("hyperv-overlay-manager".to_string()));
    }

    /// Adds an overlay of the supplied `kind` with the supplied `contents` over
    /// the supplied `pfn`.
    pub(super) fn add_overlay(
        self: &Arc<Self>,
        pfn: Pfn,
        kind: OverlayKind,
        contents: OverlayContents,
    ) -> Result<OverlayPage, OverlayError> {
        let mut inner = self.inner.lock().unwrap();
        inner.add_overlay(pfn, kind, contents, &self.acc_mem)?;
        Ok(OverlayPage { manager: Arc::downgrade(self), kind, pfn })
    }

    /// Removes an overlay of the supplied `kind` from the set at the supplied
    /// `pfn`.
    fn remove_overlay(
        &self,
        pfn: Pfn,
        kind: OverlayKind,
    ) -> Result<(), OverlayError> {
        self.inner
            .lock()
            .unwrap()
            .remove_overlay(pfn, kind, &self.acc_mem, RemoveReturnContents::No)
            .map(|_| ())
    }

    /// Moves an overlay of the supplied `kind` from `from_pfn` to `to_pfn`.
    fn move_overlay(
        &self,
        from_pfn: Pfn,
        to_pfn: Pfn,
        kind: OverlayKind,
    ) -> Result<(), OverlayError> {
        self.inner.lock().unwrap().move_overlay(
            from_pfn,
            to_pfn,
            kind,
            &self.acc_mem,
        )
    }

    pub(super) fn export(&self) -> migrate::HyperVOverlaysV1 {
        self.inner.lock().unwrap().export()
    }

    pub(super) fn import(
        self: &Arc<Self>,
        payload: migrate::HyperVOverlaysV1,
    ) -> Result<Vec<OverlayPage>, MigrateStateError> {
        self.inner.lock().unwrap().import(self, payload).map_err(Into::into)
    }
}

pub mod migrate {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use crate::migrate::{Schema, SchemaId};

    use super::OverlayKind;

    /// A kind of overlay that can appear in a saved overlay manager payload.
    ///
    /// NOTE: It is not safe to remove variants from this enum, because older
    /// Propolis versions may specify them during a migration. Uplevel versions
    /// that don't support an older overlay kind must decide what to do with
    /// overlays of that kind (ignore them, translate them, fail migration,
    /// etc.).
    #[derive(Debug, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
    #[serde(tag = "kind", content = "value")]
    pub enum MigratedOverlayKind {
        Hypercall,

        #[cfg(test)]
        Test(u8),
    }

    impl From<OverlayKind> for MigratedOverlayKind {
        fn from(value: OverlayKind) -> Self {
            match value {
                OverlayKind::Hypercall => Self::Hypercall,

                #[cfg(test)]
                OverlayKind::Test(n) => Self::Test(n),
            }
        }
    }

    impl From<MigratedOverlayKind> for OverlayKind {
        fn from(value: MigratedOverlayKind) -> Self {
            match value {
                MigratedOverlayKind::Hypercall => Self::Hypercall,

                #[cfg(test)]
                MigratedOverlayKind::Test(n) => Self::Test(n),
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    pub struct OverlaySetV1 {
        pub(super) original_contents: Vec<u8>,
        pub(super) active: MigratedOverlayKind,
        pub(super) pending: BTreeMap<MigratedOverlayKind, Vec<u8>>,
    }

    impl std::fmt::Debug for OverlaySetV1 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let Self { active, pending, original_contents: _ } = self;
            f.debug_struct("OverlaySetV1")
                .field("original_contents", &"<page redacted>")
                .field("active", &active)
                .field("pending", &pending.keys())
                .finish()
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct HyperVOverlaysV1 {
        pub(super) overlays: BTreeMap<u64, OverlaySetV1>,
    }

    impl Schema<'_> for HyperVOverlaysV1 {
        fn id() -> SchemaId {
            ("hyperv-overlays", 1)
        }
    }
}

/// Helpers for dealing with page frame numbers (guest physical page numbers, or
/// PFNs). These help to avoid page alignment checks that would otherwise be
/// necessary when dealing with full physical addresses.
mod pfn {
    use crate::{
        common::{GuestRegion, PAGE_SIZE},
        vmm::{MemCtx, Pfn, SubMapping},
    };

    use super::{OverlayContents, OverlayError};

    /// A mapping of a page of guest memory with a particular PFN.
    pub(super) struct MappedPfn<'a> {
        pfn: Pfn,
        mapping: SubMapping<'a>,
    }

    impl<'a> MappedPfn<'a> {
        /// Creates a new page-sized mapping of the supplied PFN.
        pub(super) fn new(
            pfn: Pfn,
            memctx: &'a crate::accessors::Guard<'a, MemCtx>,
        ) -> Result<Self, OverlayError> {
            let gpa = pfn.addr();
            let mapping = memctx
                .readwrite_region(&GuestRegion(gpa, PAGE_SIZE))
                .ok_or(OverlayError::AddressInaccessible(gpa.0))?;

            Ok(Self { pfn, mapping })
        }

        /// Yields this mapping's PFN.
        pub(super) fn pfn(&self) -> Pfn {
            self.pfn
        }

        /// Writes the supplied overlay page to this mapping's guest physical
        /// page.
        ///
        /// # Panics
        ///
        /// Panics if the mapping is inaccessible or if the page was only
        /// partially written.
        pub(super) fn write_page(&mut self, page: &OverlayContents) {
            assert_eq!(
                self.mapping
                    .write_bytes(page.0.as_slice())
                    .expect("PFN mappings are always accessible"),
                PAGE_SIZE
            );
        }

        /// Reads from the mapped page into the supplied buffer.
        ///
        /// # Panics
        ///
        /// Panics if the mapping is inaccessible or if the page was only
        /// partially read.
        pub(super) fn read_page(&mut self, page: &mut OverlayContents) {
            assert_eq!(
                self.mapping
                    .read_bytes(page.0.as_mut_slice())
                    .expect("PFN mappings are always accessible"),
                PAGE_SIZE
            );
        }
    }
}

#[cfg(test)]
mod test {
    use crate::vmm::{PhysMap, VmmHdl, MAX_PHYSMEM};

    use super::*;

    impl super::OverlayContents {
        fn filled(fill: u8) -> Self {
            Self(Box::new([fill; PAGE_SIZE]))
        }
    }

    /// A context for overlay tests, which includes a fake guest memory.
    struct TestCtx {
        manager: Arc<OverlayManager>,
        _vmm_hdl: Arc<VmmHdl>,
        _physmem: PhysMap,
        acc_mem: MemAccessor,
    }

    impl TestCtx {
        /// Creates a new test context with 1 MiB of addressable memory (i.e., a
        /// maximum PFN of 0xFF).
        fn new() -> Self {
            let hdl = Arc::new(VmmHdl::new_test(1024 * 1024).unwrap());
            let mut map = PhysMap::new(MAX_PHYSMEM, hdl.clone());
            map.add_test_mem("test-ram".to_string(), 0, 1024 * 1024).unwrap();
            let acc_mem = MemAccessor::new(map.memctx());

            let mgr = OverlayManager::new();
            mgr.attach(&acc_mem);
            TestCtx { manager: mgr, _vmm_hdl: hdl, _physmem: map, acc_mem }
        }

        /// Writes the supplied `contents` to the supplied `pfn` in the
        /// context's guest memory.
        fn write_page(&self, pfn: Pfn, contents: &OverlayContents) {
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = MappedPfn::new(pfn, &memctx).unwrap();
            mapping.write_page(&contents);
        }

        /// Asserts that page `pfn` of the context's memory is filled with
        /// the supplied `fill` bytes.
        fn assert_pfn_has_fill(&self, pfn: Pfn, fill: u8) {
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = MappedPfn::new(pfn, &memctx).unwrap();
            let mut contents = OverlayContents::default();
            mapping.read_page(&mut contents);
            assert_eq!(
                contents.0.as_slice(),
                [fill; PAGE_SIZE],
                "guest memory at pfn {pfn} has unexpected fill byte {:#x}",
                (contents.0)[0]
            );
        }

        /// Asserts that page `pfn` of the context's memory is filled with
        /// one of the supplied `fill` bytes.
        fn assert_pfn_fill_is_one_of(&self, pfn: Pfn, fills: &[u8]) {
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = MappedPfn::new(pfn, &memctx).unwrap();
            let mut contents = OverlayContents::default();
            mapping.read_page(&mut contents);

            assert!(
                fills.iter().any(|f| contents.0.as_slice() == [*f; PAGE_SIZE]),
                "guest memory at pfn {pfn} has unexpected fill byte {:#x}",
                (contents.0)[0]
            )
        }
    }

    /// Tests that adding an overlay page causes its contents to appear in guest
    /// memory.
    #[test]
    fn basic_add() {
        let ctx = TestCtx::new();
        let pfn = Pfn::new_unchecked(0x10);
        let contents = OverlayContents::filled(0xAB);

        let _page = ctx
            .manager
            .add_overlay(pfn, OverlayKind::Test(0), contents)
            .unwrap();

        ctx.assert_pfn_has_fill(pfn, 0xAB);
    }

    /// Tests that moving an overlay page from one PFN to another causes its
    /// contents to move from the old PFN to the new one and that the old page's
    /// contents are restored.
    #[test]
    fn basic_move() {
        let ctx = TestCtx::new();
        let pfn1 = Pfn::new_unchecked(0x10);
        let pfn2 = Pfn::new_unchecked(0x20);
        let contents = OverlayContents::filled(0xCD);

        let mut page = ctx
            .manager
            .add_overlay(pfn1, OverlayKind::Test(0), contents)
            .unwrap();

        ctx.assert_pfn_has_fill(pfn1, 0xCD);
        page.move_to(pfn2).unwrap();
        ctx.assert_pfn_has_fill(pfn1, 0);
        ctx.assert_pfn_has_fill(pfn2, 0xCD);

        drop(page);
        ctx.assert_pfn_has_fill(pfn2, 0);
    }

    /// Tests that removing the last overlay for a given physical page restores
    /// its original contents.
    #[test]
    fn underlay_restored_after_drop() {
        let ctx = TestCtx::new();
        let pfn = Pfn::new_unchecked(0x10);
        let original = OverlayContents::filled(0x11);
        let overlaid = OverlayContents::filled(0x99);
        ctx.write_page(pfn, &original);

        let page = ctx
            .manager
            .add_overlay(pfn, OverlayKind::Test(0), overlaid)
            .unwrap();

        ctx.assert_pfn_has_fill(pfn, 0x99);
        drop(page);
        ctx.assert_pfn_has_fill(pfn, 0x11);
    }

    /// Tests that the manager rejects requests to add an overlay with a PFN
    /// outside of the guest's physical address range.
    #[test]
    fn out_of_bounds_pfn() {
        let ctx = TestCtx::new();
        ctx.manager
            .add_overlay(
                Pfn::new_unchecked(0xFFFFF),
                OverlayKind::Test(0),
                OverlayContents::filled(0xFF),
            )
            .unwrap_err();
    }

    /// Tests that the manager rejects requests to have more than one overlay of
    /// a given kind at a given PFN. (This invariant is required for overlay
    /// pages to move and remove themselves correctly.)
    #[test]
    fn duplicate_kind_at_pfn() {
        let ctx = TestCtx::new();
        let pfn = Pfn::new_unchecked(0x20);
        let kind = OverlayKind::Test(0);

        let page = ctx
            .manager
            .add_overlay(pfn, kind, OverlayContents::filled(0x22))
            .unwrap();

        ctx.manager
            .add_overlay(pfn, kind, OverlayContents::filled(0x23))
            .unwrap_err();

        drop(page);
        ctx.manager
            .add_overlay(pfn, kind, OverlayContents::filled(0x23))
            .unwrap();
    }

    /// Test that a page with multiple overlays always displays the contents of
    /// one of those overlays (and that the original page contents are restored
    /// when all the overlays are gone).
    #[test]
    fn multiple_overlays() {
        let ctx = TestCtx::new();
        let pfn = Pfn::new_unchecked(0x40);
        let fills = [1, 2, 3, 4];
        let mut pages: Vec<_> = fills
            .iter()
            .map(|i| {
                ctx.manager
                    .add_overlay(
                        pfn,
                        OverlayKind::Test(*i),
                        OverlayContents::filled(*i),
                    )
                    .unwrap()
            })
            .collect();

        ctx.assert_pfn_fill_is_one_of(pfn, &fills);
        while let Some(page) = pages.pop() {
            drop(page);
            if !pages.is_empty() {
                ctx.assert_pfn_fill_is_one_of(pfn, &fills);
            } else {
                ctx.assert_pfn_has_fill(pfn, 0);
            }
        }
    }

    /// Tests that removing a nonexistent overlay from a PFN fails without
    /// disrupting any existing overlays at that PFN.
    #[test]
    fn remove_nonexistent_overlay() {
        let ctx = TestCtx::new();
        let pfn = Pfn::new_unchecked(0x30);

        let _page = ctx
            .manager
            .add_overlay(
                pfn,
                OverlayKind::Test(0),
                OverlayContents::filled(0x70),
            )
            .unwrap();

        ctx.manager.remove_overlay(pfn, OverlayKind::Test(1)).unwrap_err();

        ctx.assert_pfn_has_fill(pfn, 0x70);
    }
}
