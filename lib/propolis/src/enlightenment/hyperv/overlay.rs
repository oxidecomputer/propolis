// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers for dealing with overlays onto guest physical pages.

use crate::{
    accessors::MemAccessor,
    common::{GuestAddr, GuestRegion, PAGE_SIZE},
};

pub(super) struct OverlayPage {
    mem_acc: MemAccessor,
    addr: GuestAddr,
    old_contents: Box<[u8; PAGE_SIZE]>,
}

impl OverlayPage {
    pub(super) fn create(
        mem_acc: MemAccessor,
        addr: GuestAddr,
        contents: &[u8; PAGE_SIZE],
    ) -> Option<Self> {
        let mem_ctx = mem_acc
            .access()
            .expect("overlay pages can always access guest memory");

        let region = GuestRegion(addr, PAGE_SIZE);
        let mapping = mem_ctx.read_write_region(&region)?;

        let mut old_contents = Box::new([0u8; PAGE_SIZE]);
        let bytes_read =
            mapping.read_bytes(old_contents.as_mut_slice()).ok()?;
        if bytes_read != PAGE_SIZE {
            return None;
        }

        let bytes_written = mapping
            .write_bytes(contents)
            .expect("mapping is at least PAGE_SIZE and is writable");

        assert_eq!(bytes_written, PAGE_SIZE);

        Some(Self { mem_acc, addr, old_contents })
    }

    pub(super) fn move_to(&mut self, new_addr: GuestAddr) -> bool {
        // Do nothing if the page isn't actually moving.
        if new_addr == self.addr {
            return true;
        }

        let mem_ctx = self
            .mem_acc
            .access()
            .expect("overlay pages can always access guest memory");

        // Map the new overlay page. This is the only fallible operation in this
        // routine, since the old overlay had a valid mapping.
        let new_region = GuestRegion(self.addr, PAGE_SIZE);
        let Some(new_mapping) = mem_ctx.read_write_region(&new_region) else {
            return false;
        };

        // Back up the contents of the new overlay page.
        let mut new_saved = [0u8; PAGE_SIZE];
        assert_eq!(
            new_mapping
                .read_bytes(new_saved.as_mut_slice())
                .expect("new overlay page is readable"),
            PAGE_SIZE
        );

        // Read the overlaid data from the old mapping and write it to the new
        // page.
        let old_region = GuestRegion(self.addr, PAGE_SIZE);
        let old_mapping = mem_ctx
            .read_write_region(&old_region)
            .expect("old overlay page is readable and writable");

        let mut to_move = [0u8; PAGE_SIZE];
        assert_eq!(
            old_mapping
                .read_bytes(to_move.as_mut_slice())
                .expect("old overlay page is readable"),
            PAGE_SIZE
        );

        assert_eq!(
            new_mapping
                .write_bytes(to_move.as_slice())
                .expect("new overlay page is writable"),
            PAGE_SIZE
        );

        // Restore the data from the old mapping to the old page.
        assert_eq!(
            old_mapping
                .write_bytes(self.old_contents.as_slice())
                .expect("old overlay page is writable"),
            PAGE_SIZE
        );

        // Save the new mapping's previous data so it can be restored later.
        self.old_contents.copy_from_slice(&to_move);

        true
    }

    fn remove(&mut self) {
        let mem_ctx = self
            .mem_acc
            .access()
            .expect("overlay pages can always access guest memory");

        let region = GuestRegion(self.addr, PAGE_SIZE);
        let mapping = mem_ctx
            .read_write_region(&region)
            .expect("overlay page was writable before");

        let bytes_written = mapping
            .write_bytes(self.old_contents.as_slice())
            .expect("overlay region is writable and page-sized");

        assert_eq!(bytes_written, PAGE_SIZE);
    }
}

impl Drop for OverlayPage {
    fn drop(&mut self) {
        self.remove();
    }
}
