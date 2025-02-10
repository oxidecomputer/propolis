// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for hypervisor overlay pages.
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

use std::{
    collections::{btree_map::Entry, BTreeMap},
    sync::{Arc, Mutex, Weak},
};

use thiserror::Error;

use crate::{accessors::MemAccessor, common::PAGE_SIZE};

use self::pfn::{MappedPfn, Pfn};

/// An error that can be returned from an overlay page operation.
#[derive(Debug, Error)]
pub enum OverlayError {
    /// The guest memory context can't be accessed right now. Generally this
    /// means that the caller is trying to create an overlay too early (i.e.,
    /// before the overlay manager is attached to the memory hierarchy) or too
    /// late (i.e., after the VM has shut down).
    #[error("guest memory is currently inaccessible")]
    GuestMemoryInaccessible,

    /// The supplied PFN is too large to be the page number for a 4-KiB page.
    #[error("overlay target PFN {0:#x} is too large for a 4K page")]
    PfnTooLarge(u64),

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
}

/// The contents of a 4 KiB page. These are boxed so that this type can be
/// embedded in a struct that's put into a contiguous collection without putting
/// entire pages between collection members.
#[derive(Clone)]
pub(super) struct OverlayContents(pub(super) Box<[u8; PAGE_SIZE]>);

impl Default for OverlayContents {
    fn default() -> Self {
        Self(Box::new([0u8; PAGE_SIZE]))
    }
}

impl std::fmt::Debug for OverlayContents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OverlayContents").field(&"<page redacted>").finish()
    }
}

/// A kind of overlay page.
#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub(super) enum OverlayKind {
    Hypercall,

    #[cfg(test)]
    Test(u8),
}

/// A registered overlay page, held by a user of this module. The overlay is
/// removed when this page is dropped.
#[derive(Debug)]
pub(super) struct OverlayPage {
    /// A back reference to the page's overlay manager.
    manager: Weak<OverlayManager>,

    /// The kind of overlay this is.
    kind: OverlayKind,

    /// The PFN at which this overlay is currently applied.
    pfn: Pfn,
}

impl OverlayPage {
    pub(super) fn move_to(&mut self, new_pfn: u64) -> Result<(), OverlayError> {
        let manager = self
            .manager
            .upgrade()
            .expect("can only move overlay pages with an active manager");

        let new_pfn = Pfn::new(new_pfn)?;
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
        // Once the VM *is* halted, no one will access guest memory again,
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

        let mut mapped = pfn.map(&memctx)?;
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
    fn remove_overlay(
        &mut self,
        pfn: Pfn,
        kind: OverlayKind,
        acc_mem: &MemAccessor,
    ) -> Result<(), OverlayError> {
        let Entry::Occupied(mut set) = self.overlays.entry(pfn) else {
            return Err(OverlayError::NoOverlaysActive(pfn.into()));
        };

        if set.get().active == kind {
            let memctx = acc_mem
                .access()
                .ok_or(OverlayError::GuestMemoryInaccessible)?;

            let mut mapped =
                pfn.map(&memctx).expect("active overlay PFNs can be mapped");

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

            Ok(())
        } else {
            // This overlay kind isn't active for this page. If it's pending,
            // it's sufficient just to remove its entry from the pending map,
            // since it has had no effect on guest memory.
            //
            // If there's no pending overlay of this kind, the caller goofed.
            set.get_mut()
                .pending
                .remove(&kind)
                .ok_or(OverlayError::NoOverlayOfKind(pfn.into(), kind))
                .map(|_| ())
        }
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

        // Move the overlay by removing it from its old location and applying it
        // to its new one. This will only work if an overlay of the requested
        // kind does in fact exist at the source location, which means that it
        // must have an active overlay set.
        let Some(from_set) = self.overlays.get_mut(&from_pfn) else {
            return Err(OverlayError::NoOverlaysActive(from_pfn.into()));
        };

        // The overlay contents may be stored in the table itself (for pending
        // overlays) or in guest memory (for active overlays). Before bothering
        // to obtain them, create a mapping for the target page to verify that
        // the target PFN is valid. This mapping will be used later if this is
        // the first overlay for the target PFN.
        let mut to_mapping = to_pfn.map(&memctx)?;

        // Get the page contents to transfer, but don't actually change the
        // tables yet, since applying the new overlay might still fail if there
        // is an existing overlay of this kind at the target PFN.
        //
        // It's possible to do this check now, modify the tables in-place, and
        // then assert that adding the new overlay succeeded, but this sequence
        // allows this function to reuse the existing overlay removal code. This
        // is unlikely to be a bottleneck in practice because guests change
        // their overlay locations so infrequently (and any guest that spends
        // all its time moving overlays around has, in some ways, asked for
        // it...).
        let contents = if from_set.active == kind {
            let mut from_mapping = from_pfn
                .map(&memctx)
                .expect("active overlay PFNs can be mapped");
            let mut contents = OverlayContents::default();
            from_mapping.read_page(&mut contents);
            contents
        } else if let Some(pending_contents) = from_set.pending.get(&kind) {
            pending_contents.clone()
        } else {
            return Err(OverlayError::NoOverlayOfKind(from_pfn.into(), kind));
        };

        self.add_overlay_using_mapping(&mut to_mapping, kind, contents)?;
        self.remove_overlay(from_pfn, kind, acc_mem)
            .expect("already found and read existing overlay");

        Ok(())
    }
}

/// An overlay tracker that adds, removes, and moves overlays.
///
/// # Usage requirements
///
/// After calling [`OverlayManager::new`], the caller must call
/// [`OverlayManager::attach`] to attach the manager's memory accessor to a VM's
/// memory hierarchy before it can add any overlays.
///
/// The [`OverlayPage`] structs returned from [`OverlayManager::add_overlay`]
/// will attempt to remove themselves when they are dropped and assert that they
/// can do so successfully. This will trigger a panic during VM shutdown if an
/// overlay page with an active manager is dropped after the VM's memory context
/// has been released. To avoid this, a Hyper-V stack that owns an overlay
/// manager can drop its reference to the manager in its [`Lifecycle::halt`]
/// callout; at that point no vCPUs are active or will become active, so this
/// should remove the last strong reference to the manager, after which it is
/// safe to drop the pages it produced.
///
/// [`Lifecycle::halt`]: crate::lifecycle::Lifecycle::halt
pub(super) struct OverlayManager {
    inner: Mutex<ManagerInner>,
    acc_mem: MemAccessor,
}

impl OverlayManager {
    /// Creates a new overlay manager.
    pub(super) fn new() -> Arc<Self> {
        let acc_mem = MemAccessor::new_orphan();
        Arc::new(Self { inner: Mutex::new(ManagerInner::default()), acc_mem })
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
        pfn: u64,
        kind: OverlayKind,
        contents: OverlayContents,
    ) -> Result<OverlayPage, OverlayError> {
        let pfn = Pfn::new(pfn)?;
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
        self.inner.lock().unwrap().remove_overlay(pfn, kind, &self.acc_mem)
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
}

/// Helpers for dealing with page frame numbers (guest physical page numbers, or
/// PFNs). These help to avoid page alignment checks that would otherwise be
/// necessary when dealing with full physical addresses.
mod pfn {
    use crate::{
        common::{GuestAddr, GuestRegion, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE},
        vmm::SubMapping,
    };

    use super::{OverlayContents, OverlayError};

    /// A 52-bit physical page number.
    #[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
    pub(super) struct Pfn(u64);

    impl std::fmt::Debug for Pfn {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Pfn({:x})", self.0)
        }
    }

    impl std::fmt::Display for Pfn {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:x}", self.0)
        }
    }

    impl Pfn {
        /// Creates a new PFN wrapper from a physical page number, returning an
        /// error if the proposed page number cannot be shifted to produce a
        /// 64-bit physical address.
        pub(super) fn new(pfn: u64) -> Result<Self, OverlayError> {
            if pfn > (PAGE_MASK as u64 >> PAGE_SHIFT) {
                Err(OverlayError::PfnTooLarge(pfn))
            } else {
                Ok(Self(pfn))
            }
        }

        #[cfg(test)]
        pub(super) fn new_unchecked(pfn: u64) -> Self {
            Self::new(pfn).unwrap()
        }

        /// Yields the 64-bit guest physical address corresponding to this PFN.
        pub(super) fn gpa(&self) -> GuestAddr {
            GuestAddr(self.0 << PAGE_SHIFT)
        }

        /// Creates a read-write guest memory mapping for this PFN.
        pub(super) fn map<'a>(
            &self,
            memctx: &'a crate::accessors::Guard<'a, crate::vmm::MemCtx>,
        ) -> Result<MappedPfn<'a>, OverlayError> {
            let gpa = self.gpa();
            let mapping = memctx
                .readwrite_region(&GuestRegion(gpa, PAGE_SIZE))
                .ok_or(OverlayError::AddressInaccessible(gpa.0))?;

            Ok(MappedPfn { pfn: *self, mapping })
        }
    }

    impl From<Pfn> for u64 {
        fn from(value: Pfn) -> Self {
            value.0
        }
    }

    /// A mapping of a page of guest memory with a particular PFN.
    pub(super) struct MappedPfn<'a> {
        pfn: Pfn,
        mapping: SubMapping<'a>,
    }

    impl MappedPfn<'_> {
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
        fn write_page(&self, pfn: u64, contents: &OverlayContents) {
            let pfn = Pfn::new_unchecked(pfn);
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = pfn.map(&memctx).unwrap();
            mapping.write_page(&contents);
        }

        /// Asserts that page `pfn` of the context's memory is filled with
        /// the supplied `fill` bytes.
        fn assert_pfn_has_fill(&self, pfn: u64, fill: u8) {
            let pfn = Pfn::new_unchecked(pfn);
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = pfn.map(&memctx).unwrap();
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
        fn assert_pfn_fill_is_one_of(&self, pfn: u64, fills: &[u8]) {
            let pfn = Pfn::new_unchecked(pfn);
            let memctx = self.acc_mem.access().unwrap();
            let mut mapping = pfn.map(&memctx).unwrap();
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
        let pfn = 0x10;
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
        let pfn1 = 0x10;
        let pfn2 = 0x20;
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
        let pfn = 0x10;
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
                0xFFFFF,
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
        let pfn = 0x20;
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
        let pfn = 0x40;
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
}
