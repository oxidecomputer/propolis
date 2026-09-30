// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Define a newtype for a kind of run-time identifier. Exact semantics for a
// given identifier depend on what uses it, but they are expected to be used
// only in places internal to a VM's instantiation. DTrace probes for VM
// operations, device state that is expected to be lost across migration, that
// kind of thing.
//
// As an example of what not to do, these identifiers must not be used as field
// on Oximeter metrics: because the identifier for some item in the VM may
// change across instantiations, the field may become meaningless across
// stop/start or migration. In such cases a more persistent identifier (PCI BDF,
// NIC MAC, vCPU ACPI ID, etc) must be used instead.
//
// If an item needs stable identifiers, this is not the tool to use.
//
// This macro takes syntax matching the newtype definition primarily so that
// grepping for the newtype like `struct DeviceId` finds corresponding macro
// invocations.
macro_rules! define_id {
    {
        $(#[$meta_items:meta])*
        pub struct $id_name:ident($visibility:vis u32);
    } => {
        ::paste::paste! {
            $(#[$meta_items])*
            pub struct $id_name($visibility u32);

            impl $id_name {
                pub const INVALID: $id_name = $id_name(u32::MAX);
                pub fn new() -> Self {
                    static [<_NEXT_ $id_name:upper>]: ::std::sync::atomic::AtomicU32 =
                        ::std::sync::atomic::AtomicU32::new(0);

                    let id = [<_NEXT_ $id_name:upper>].fetch_add(
                        1,
                        ::std::sync::atomic::Ordering::Relaxed
                    );
                    $id_name(id)
                }
            }
        }
    }
}
pub(crate) use define_id;
