// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Runtime identifiers for block devices and their backends.
//!
//! Devices that support block backends and the block backends themselves are
//! independent identifiers, and only become related when an item of each type
//! is connected via [`block::attach`].
//!
//! Devices in particular may have multiple identifiers, some from this module
//! and some from others. As one example, [`propolis::hw::nvme::NvmeCtrl`] has a
//! `device_id` distinguishing *instances of the NVMe controller* across a VM,
//! while the `PciNvme` which has an NVMe controller also has `block_attach`
//! with a `device_id` distinguishing *instances of block devices* across a VM.
//!
//! ## Limitations
//!
//! A consumer of `propolis` is free to construct devices supporting block
//! backends in any order, and may happen to construct block backends in any
//! different arbitrary order. Attaching the two kinds of item together is also
//! up to the consumer of `propolis`, and there is no requirement that a
//! particular block backend must be connected to a particular device.
//!
//! Consequently, these identifiers are not stable for use in migration of a VM,
//! and must not be used in a way visible to a VM. They are unsuitable for
//! emulated device serial numbers, model numbers, etc. The destination
//! `propolis` may construct the same set of devices in a different order,
//! resulting in different run-time identifiers for a device at the same
//! location.

use crate::util::id::define_id;

define_id! {
    /// Numbering across block devices means that a block `DeviceId` and the
    /// queue ID in a block attachment are unique across a VM.
    #[derive(Copy, Clone)]
    pub struct DeviceId(pub(crate) u32);
}

define_id! {
    /// Block backends are numbered distinctly across a VM, but may not
    /// be created in the same order as devices. The `block_attach` probe fires
    /// when a `DeviceId` and `BackendId` become associated.
    #[derive(Copy, Clone)]
    pub struct BackendId(pub(crate) u32);
}
