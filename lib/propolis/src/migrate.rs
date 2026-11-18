// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::vmm::MemCtx;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors encountered while trying to export/import device state.
#[derive(Debug, Error)]
pub enum MigrateStateError {
    /// The device doesn't support live migration.
    #[error("device not migratable")]
    NonMigratable,

    /// The device isn't in a state where its state can be exported. Because
    /// fully-initialized devices should be able to pause and export their state
    /// at any time, this generally means that the device was asked to export
    /// its state before it was fully initialized.
    #[error("device's state is not ready to be exported")]
    NotReadyForExport,

    /// I/O Error encounted while performing import/export
    #[error("IO Error")]
    Io(#[from] std::io::Error),

    /// Encountered an error trying to deserialize the device state during import.
    #[error("could not deserialize device state: {0}")]
    DeserializationFailed(String),

    /// The device failed to import the deserialized device state.
    #[error("failed to apply deserialized device state: {0}")]
    ImportFailed(String),

    /// State of kind/version suitable for import not found
    #[error("failed to find suitable import payload")]
    DataMissing,

    /// The kind and/or version of payload was not expected
    #[error("kind/version of payload not expected: {0} v{1}")]
    UnexpectedPayload(String, u32),
}

impl From<erased_serde::Error> for MigrateStateError {
    fn from(err: erased_serde::Error) -> Self {
        MigrateStateError::DeserializationFailed(err.to_string())
    }
}

/// Type representing the migration support (if any) for a given device.
pub enum Migrator<'a> {
    /// The device is not migratable
    NonMigratable,

    /// No device specific logic is needed
    Empty,

    /// Migration state for the device consists of a single payload.
    ///
    /// The device may be capable of handing differing formats and/or versions
    /// of said payload, but only one at a time is expected for a given device
    /// during migration.
    Single(&'a dyn MigrateSingle),

    /// Migration state for the device consists of multiple payloads.  This is
    /// the case for more complex devices where emulation is composed from
    /// several abstractions.
    ///
    /// One example would be pci-virtio-block, where one payload contains to the
    /// emulated PCI state, while another contains the virtio state (virtqueues,
    /// etc).
    Multi(&'a dyn MigrateMulti),
}

/// A device which can be migrated using a single typed payload to represent its
/// internal state.
pub trait MigrateSingle: Send + Sync + 'static {
    fn export(
        &self,
        ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError>;
    fn import(
        &self,
        offer: PayloadOffer,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
}

/// A device which can be migrated using multiple differently-typed payloads to
/// represent its internal state.
pub trait MigrateMulti: Send + Sync {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
    fn import(
        &self,
        offer: &mut PayloadOffers,
        ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError>;
}

/// Additional (borrowed) context data used during device import and export.
pub struct MigrateCtx<'a> {
    pub mem: &'a MemCtx,
}

/// Device state payload (of a given kind/version) offered to the import logic
/// for that device during a migration.
pub struct PayloadOffer<'a> {
    pub kind: &'a str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Deserializer<'a> + 'a>,
}
impl<'a> PayloadOffer<'a> {
    /// Attempt to parse the data in this payload if the offer matches the
    /// kind/version of a specified Schema
    pub fn parse<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        if !self.matches::<T>() {
            return Err(MigrateStateError::UnexpectedPayload(
                self.kind.into(),
                self.version,
            ));
        }
        let res = erased_serde::deserialize(&mut self.payload)?;
        Ok(res)
    }

    /// Returns `true` if the `kind` and `version` held in this `PayloadOffer`
    /// match those defined for a provided Schema.
    fn matches<'x, T: Schema<'x>>(&self) -> bool {
        let id = T::id();
        id.0 == self.kind && id.1 == self.version
    }
}

/// Collection of [`PayloadOffer`] instances, as provided to device state import
/// logic during a migration.
pub struct PayloadOffers<'a>(Vec<PayloadOffer<'a>>);
impl<'a> PayloadOffers<'a> {
    /// Create new [`PayloadOffers`] from iterator of [`PayloadOffer`]
    /// instances.
    pub fn new(items: impl IntoIterator<Item = PayloadOffer<'a>>) -> Self {
        Self(Vec::from_iter(items))
    }

    /// Attempt to take a payload from the contained offers, provided that it
    /// matches the specified [`Schema`](trait@Schema).
    pub fn take<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        self.take_schema(T::id())
            .ok_or_else(|| MigrateStateError::DataMissing)?
            .parse()
    }

    /// Returns `true` if all of the payload offers been consumed via
    /// [`Self::take()`].
    pub fn is_consumed(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the remaining PayloadOffer instances which have not been
    /// consumed.
    ///
    /// Intended for use by the logic driving the migration itself to determine
    /// if a given device importation failed to consume all of its payload(s).
    pub fn remaining(self) -> Remaining<'a> {
        Remaining(self.0.into_iter())
    }

    fn take_schema(&mut self, id: SchemaId) -> Option<PayloadOffer<'a>> {
        let mut search = self.0.iter().enumerate().filter(|(_idx, offer)| {
            offer.kind == id.0 && offer.version == id.1
        });

        // Success if we find one, and only one, matching the criteria
        if let (Some((idx, _offer)), None) = (search.next(), search.next()) {
            return Some(self.0.remove(idx));
        }

        None
    }
}

pub struct Remaining<'a>(std::vec::IntoIter<PayloadOffer<'a>>);
impl<'a> Iterator for Remaining<'a> {
    type Item = PayloadOffer<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Device state payload (of a given kind/version) output as part of the export
/// logic for that device during a migration.
///
/// Easiest to safely instantiate via the [`From`] trait implemented for
/// [`Schema`] to ensure that the kind/version matches the serialized payload.
pub struct PayloadOutput {
    pub kind: &'static str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Serialize>,
}

pub struct PayloadOutputs(Vec<PayloadOutput>);
impl PayloadOutputs {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add a provided [`PayloadOutput`] to be included in the payload(s) for a
    /// given exported device
    pub fn push(
        &mut self,
        output: PayloadOutput,
    ) -> Result<(), MigrateStateError> {
        self.0.push(output);
        Ok(())
    }
}
impl IntoIterator for PayloadOutputs {
    type Item = PayloadOutput;

    type IntoIter = OutputIter;

    fn into_iter(self) -> Self::IntoIter {
        OutputIter(self.0.into_iter())
    }
}
// Hide the internal implementation details of PayloadOutputs by providing a
// wrapper type for its output iterator.
pub struct OutputIter(std::vec::IntoIter<PayloadOutput>);
impl Iterator for OutputIter {
    type Item = PayloadOutput;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Combination of kind (`&str`) and version (u32) which identifies a specific
/// data schema for device migration state.
pub type SchemaId = (&'static str, u32);

/// Define the type (kind) and version for a migration payload data structure.
pub trait Schema<'de>: Serialize + Deserialize<'de> + Sized + 'static {
    /// The [`SchemaId`] associated with a given device state data type.
    ///
    /// This would be `const` if such functions were allowed in traits without
    /// an unstable rust feature.
    fn id() -> SchemaId;
}

impl<'a, T: Schema<'a>> From<T> for PayloadOutput {
    fn from(value: T) -> Self {
        let id = T::id();
        PayloadOutput { kind: id.0, version: id.1, payload: Box::new(value) }
    }
}

#[cfg(test)]
pub mod test {
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;
    use crate::accessors::MemAccessor;
    use crate::common::{GuestAddr, MB};
    use crate::lifecycle::Lifecycle;
    use crate::vmm::PhysMap;

    /// Exports migration payload from `src` using the given migration context.
    pub fn export(src: &dyn Lifecycle, ctx: &MigrateCtx) -> MigrationPayload {
        match src.migrate() {
            Migrator::Single(m) => MigrationPayload::Single(
                m.export(ctx)
                    .unwrap_or_else(|e| {
                        panic!("{} export failed: {e}", src.type_name())
                    })
                    .into(),
            ),
            Migrator::Multi(m) => {
                let mut payloads = PayloadOutputs::new();
                m.export(&mut payloads, ctx).unwrap_or_else(|e| {
                    panic!("{} export failed: {e}", src.type_name())
                });
                MigrationPayload::Multi(
                    payloads.into_iter().map(|p| p.into()).collect(),
                )
            }
            Migrator::Empty => MigrationPayload::Empty,
            Migrator::NonMigratable => {
                panic!("{} is not migratable", src.type_name())
            }
        }
    }

    /// Imports migration payload into `dest` with the given migration context.
    pub fn import(
        dest: &dyn Lifecycle,
        payload: MigrationPayload,
        ctx: &MigrateCtx,
    ) {
        match dest.migrate() {
            Migrator::Single(m) => {
                let payload = payload.into_single();
                let mut deser = ron::Deserializer::from_str(&payload.data)
                    .expect("valid payload");
                let offer = PayloadOffer {
                    kind: &payload.kind,
                    version: payload.version,
                    payload: Box::new(<dyn erased_serde::Deserializer>::erase(
                        &mut deser,
                    )),
                };
                m.import(offer, ctx).unwrap_or_else(|e| {
                    panic!("{} import failed: {e}", dest.type_name())
                });
            }
            Migrator::Multi(m) => {
                let payloads = payload.into_multi();
                let mut desers = Vec::new();
                for payload in payloads.iter() {
                    desers.push(
                        ron::Deserializer::from_str(&payload.data)
                            .expect("valid payload"),
                    );
                }
                let offers_iter = payloads.iter().zip(desers.iter_mut()).map(
                    |(payload, deser)| PayloadOffer {
                        kind: &payload.kind,
                        version: payload.version,
                        payload: Box::new(
                            <dyn erased_serde::Deserializer>::erase(deser),
                        ),
                    },
                );

                let mut offers = PayloadOffers::new(offers_iter);
                m.import(&mut offers, ctx).unwrap_or_else(|e| {
                    panic!("{} import failed: {e}", dest.type_name())
                });

                let remaining = offers
                    .remaining()
                    .map(|o| (o.kind, o.version))
                    .collect::<Vec<_>>();

                assert!(
                    remaining.is_empty(),
                    "{} left offers remaining after migration: {remaining:?}",
                    dest.type_name()
                );
            }
            Migrator::Empty => match payload {
                MigrationPayload::Empty => (),
                _ => panic!("{} can't import payloads", dest.type_name()),
            },
            Migrator::NonMigratable => {
                panic!("{} is not migratable", dest.type_name())
            }
        }
    }

    /// Exports migration payload from `src` and imports it into `dest` using
    /// the given migration context.
    pub fn migrate(
        src: &dyn Lifecycle,
        dest: &dyn Lifecycle,
        ctx: &MigrateCtx,
    ) {
        import(dest, export(src, ctx), ctx)
    }

    /// Simulates a migration by exporting a device and importing the migration
    /// payload into another device, mirroring the RonV0Runner::device_state()
    /// and RonV0::import_device() methods used in propolis-server.
    ///
    /// It's possible to call [`MigrationDriver::export`] and
    /// [`MigrationDriver::import`] in isolation or use
    /// [`MigrationDriver::migrate`] to run both in sequence.
    ///
    /// [`MigrationDriver::with_mem`] returns a [`MigrationDriver`] that uses
    /// the given [`MemAccessor`] in the migration context of import and export
    /// operations.
    ///
    /// Use the [`export`], [`import`], and [`migrate`] functions to pass a
    /// [`MigrateCtx`] directly.
    pub struct MigrationDriver {
        mem: MemAccessor,
    }
    impl MigrationDriver {
        /// Creates a migration driver that uses a default migration context.
        pub fn new() -> Self {
            Self { mem: test_phys_map().finalize() }
        }

        /// Creates a migration driver that uses the provided guest memory in
        /// the migration context.
        pub fn with_mem(mem: &MemAccessor) -> Self {
            Self { mem: mem.child(None) }
        }

        /// Migrates `src` into `dest`.
        pub fn migrate(&self, src: &dyn Lifecycle, dest: &dyn Lifecycle) {
            self.run(|ctx| migrate(src, dest, ctx))
        }

        /// Exports the migration payload from `src`.
        pub fn export(&self, src: &dyn Lifecycle) -> MigrationPayload {
            self.run(|ctx| export(src, ctx))
        }

        /// Imports the migration payload into `dest`.
        pub fn import(&self, dest: &dyn Lifecycle, payload: MigrationPayload) {
            self.run(|ctx| import(dest, payload, ctx))
        }

        // Helper method to run a function using the right migration context.
        fn run<T>(&self, f: impl FnOnce(&MigrateCtx<'_>) -> T) -> T {
            let acc = self.mem.access().expect("migration driver has memory");
            f(&MigrateCtx { mem: &acc })
        }
    }

    /// Wrapper for the different types of migration payloads.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum MigrationPayload {
        Empty,
        Single(DevicePayload),
        Multi(Vec<DevicePayload>),
    }
    impl MigrationPayload {
        pub fn into_single(self) -> DevicePayload {
            match self {
                MigrationPayload::Single(p) => p,
                _ => panic!("expected single payload"),
            }
        }

        pub fn into_multi(self) -> Vec<DevicePayload> {
            match self {
                MigrationPayload::Multi(p) => p,
                _ => panic!("expected multi payload"),
            }
        }

        /// Returns migration payload parsed as `Some(T)` or `None` if the
        /// payload metadata (kind and version) doesn't match `T`. For multi
        /// migration payloads the first payload to match `T` is returned.
        ///
        /// # Panics
        ///
        /// Panics if the payload can't be parsed into `T`.
        pub fn get<'a, T: Schema<'a>>(&'a self) -> Option<T> {
            match self {
                MigrationPayload::Single(p) => p.parse(),
                MigrationPayload::Multi(ps) => {
                    ps.iter().find_map(|p| p.parse::<T>())
                }
                MigrationPayload::Empty => None,
            }
        }
    }

    /// Serialized version of the migration payload sent by devices.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct DevicePayload {
        pub kind: String,
        pub version: u32,
        pub data: String,
    }
    impl DevicePayload {
        /// Parses a [`DevicePayload`] into the struct used by the device to
        /// generate the migration payload.
        pub fn parse<'a, T: Schema<'a>>(&'a self) -> Option<T> {
            (T::id() == (self.kind.as_str(), self.version)).then(|| {
                ron::from_str(&self.data).unwrap_or_else(|e| {
                    panic!("failed to parse {:?}: {e}", self)
                })
            })
        }
    }
    impl From<PayloadOutput> for DevicePayload {
        fn from(payload: PayloadOutput) -> DevicePayload {
            DevicePayload {
                kind: payload.kind.to_string(),
                version: payload.version,
                data: ron::ser::to_string(&payload.payload).unwrap_or_else(
                    |e| {
                        panic!(
                            "failed to serialize payload {}: {e}",
                            payload.kind
                        )
                    },
                ),
            }
        }
    }

    const TEST_MEM_ADDR: GuestAddr = GuestAddr(MB as u64);

    fn test_phys_map() -> PhysMap {
        let addr = TEST_MEM_ADDR.0 as usize;

        let mut map = PhysMap::new_test(2 * MB);
        map.add_test_mem("test-ram".to_string(), addr, MB)
            .expect("can create test memory");
        map
    }

    enum TestDeviceMode {
        NonMigratable,
        Empty,
        Single,
        Multi,
    }

    /// Test device that exports/imports some local data.
    ///
    /// If mode is [`TestDeviceMode::Multi`], it also exports the value stored
    /// in the [`TEST_MEM_ADDR`] address of the memory map passed in the
    /// migration context.
    struct TestDevice {
        mode: TestDeviceMode,

        /// When true, the memory data is exported, but not imported.
        skip_offer: bool,

        /// Internal data used for test assertions.
        data: AtomicU8,

        /// Memory value read from address [`TEST_MEM_ADDR`]. Only set after
        /// import if `mode` is [`TestDeviceMode::Multi`] and `skip_offer` is
        /// `false`.
        mem: AtomicU8,
    }
    impl TestDevice {
        fn new(mode: TestDeviceMode) -> Self {
            Self {
                data: AtomicU8::new(0),
                mem: AtomicU8::new(0),
                skip_offer: false,
                mode,
            }
        }
        fn single() -> Self {
            TestDevice::new(TestDeviceMode::Single)
        }
        fn multi() -> Self {
            TestDevice::new(TestDeviceMode::Multi)
        }
        fn empty() -> Self {
            TestDevice::new(TestDeviceMode::Empty)
        }
        fn non_migratable() -> Self {
            TestDevice::new(TestDeviceMode::NonMigratable)
        }

        /// Sets the value for `skip_offer`. Only allowed if `mode` is
        /// [`TestDeviceMode::Multi`].
        fn skip_offer(mut self) -> Self {
            match self.mode {
                TestDeviceMode::Multi => {
                    self.skip_offer = true;
                    self
                }
                _ => panic!("skip offer test expects multi migration"),
            }
        }

        fn data(&self) -> u8 {
            self.data.load(Ordering::Relaxed)
        }

        fn set_data(&self, data: u8) {
            self.data.store(data, Ordering::Relaxed);
        }

        /// Returns data read from the migration context memory after an
        /// import. Only set if `mode` is [`TestDeviceMode::Multi`] and
        /// `skip_offer` is `false`.
        fn mem(&self) -> u8 {
            self.mem.load(Ordering::Relaxed)
        }
    }
    impl Lifecycle for TestDevice {
        fn type_name(&self) -> &'static str {
            "test-device"
        }

        fn migrate(&self) -> Migrator<'_> {
            match self.mode {
                TestDeviceMode::NonMigratable => Migrator::NonMigratable,
                TestDeviceMode::Single => Migrator::Single(self),
                TestDeviceMode::Multi => Migrator::Multi(self),
                TestDeviceMode::Empty => Migrator::Empty,
            }
        }
    }
    impl MigrateSingle for TestDevice {
        fn export(
            &self,
            ctx: &MigrateCtx,
        ) -> Result<PayloadOutput, MigrateStateError> {
            Ok(TestDeviceDataPayloadV1::read(self, ctx).into())
        }

        fn import(
            &self,
            mut offer: PayloadOffer,
            ctx: &MigrateCtx,
        ) -> Result<(), MigrateStateError> {
            offer.parse::<TestDeviceDataPayloadV1>()?.write(self, ctx);
            Ok(())
        }
    }
    impl MigrateMulti for TestDevice {
        fn export(
            &self,
            output: &mut PayloadOutputs,
            ctx: &MigrateCtx,
        ) -> Result<(), MigrateStateError> {
            output.push(TestDeviceDataPayloadV1::read(self, ctx).into())?;
            output.push(TestDeviceMemPayloadV1::read(self, ctx).into())?;
            Ok(())
        }

        fn import(
            &self,
            offer: &mut PayloadOffers,
            ctx: &MigrateCtx,
        ) -> Result<(), MigrateStateError> {
            offer.take::<TestDeviceDataPayloadV1>()?.write(self, ctx);
            if !self.skip_offer {
                offer.take::<TestDeviceMemPayloadV1>()?.write(self, ctx);
            }
            Ok(())
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct TestDeviceDataPayloadV1 {
        /// Value stored in the internal device data.
        data: u8,
    }
    impl Schema<'_> for TestDeviceDataPayloadV1 {
        fn id() -> SchemaId {
            ("test-device-data", 1)
        }
    }
    impl TestDeviceDataPayloadV1 {
        fn read(dev: &TestDevice, _: &MigrateCtx) -> Self {
            TestDeviceDataPayloadV1 { data: dev.data() }
        }

        fn write(self, dev: &TestDevice, _: &MigrateCtx) {
            dev.set_data(self.data);
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct TestDeviceMemPayloadV1 {
        /// Value stored in the migration context memory at address
        /// [`TEST_MEM_ADDR`].
        mem: u8,
    }
    impl Schema<'_> for TestDeviceMemPayloadV1 {
        fn id() -> SchemaId {
            ("test-device-mem", 1)
        }
    }
    impl TestDeviceMemPayloadV1 {
        fn read(_: &TestDevice, ctx: &MigrateCtx) -> Self {
            let mem = *ctx
                .mem
                .read::<u8>(TEST_MEM_ADDR)
                .expect("can read test memory");
            TestDeviceMemPayloadV1 { mem }
        }

        fn write(self, dev: &TestDevice, ctx: &MigrateCtx) {
            let mem = *ctx
                .mem
                .read::<u8>(TEST_MEM_ADDR)
                .expect("can read test memory");
            dev.mem.store(mem, Ordering::Relaxed);
        }
    }

    #[test]
    fn export_single() {
        let src = TestDevice::single();
        let payload = MigrationDriver::new().export(&src).into_single();

        assert_eq!(payload.kind, "test-device-data");
        assert_eq!(payload.version, 1);
    }

    #[test]
    fn export_multi() {
        let src = TestDevice::multi();
        let payloads = MigrationDriver::new().export(&src).into_multi();

        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].kind, "test-device-data");
        assert_eq!(payloads[0].version, 1);
        assert_eq!(payloads[1].kind, "test-device-mem");
        assert_eq!(payloads[1].version, 1);
    }

    #[test]
    fn export_empty() {
        let src = TestDevice::empty();
        let payload = MigrationDriver::new().export(&src);
        assert_eq!(payload, MigrationPayload::Empty);
    }

    #[test]
    fn export_with_mem() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let src = TestDevice::multi();

        let got = MigrationDriver::with_mem(&mem)
            .export(&src)
            .get::<TestDeviceMemPayloadV1>()
            .unwrap();
        assert_eq!(got.mem, 2);
    }

    #[test]
    fn export_with_ctx() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let ctx = MigrateCtx { mem: &mem.access().unwrap() };
        let src = TestDevice::multi();

        let got = export(&src, &ctx).get::<TestDeviceMemPayloadV1>().unwrap();
        assert_eq!(got.mem, 2);
    }

    #[test]
    fn import_single() {
        let driver = MigrationDriver::new();

        let src = TestDevice::single();
        src.set_data(2);

        let payload = driver.export(&src);
        let dest = TestDevice::single();

        driver.import(&dest, payload);
        assert_eq!(dest.data(), 2);
    }

    #[test]
    fn import_multi() {
        let driver = MigrationDriver::new();

        let src = TestDevice::multi();
        src.set_data(2);

        let payload = driver.export(&src);
        let dest = TestDevice::multi();

        driver.import(&dest, payload);
        assert_eq!(dest.data(), 2);
    }

    #[test]
    fn import_with_mem() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let src = TestDevice::multi();
        src.set_data(3);

        let driver = MigrationDriver::with_mem(&mem);
        let payload = driver.export(&src);

        // Modify memory to ensure import reads from the passed memory.
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &4);

        let dest = TestDevice::multi();

        driver.import(&dest, payload);
        assert_eq!(dest.mem(), 4);
    }

    #[test]
    fn import_with_ctx() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let src = TestDevice::multi();
        src.set_data(3);

        let ctx = MigrateCtx { mem: &mem.access().unwrap() };
        let payload = export(&src, &ctx);

        // Modify memory to ensure import reads from the passed memory.
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &4);

        let dest = TestDevice::multi();

        import(&dest, payload, &ctx);
        assert_eq!(dest.mem(), 4);
    }

    #[test]
    fn migrate_single() {
        let src = TestDevice::single();
        src.set_data(2);

        let dest = TestDevice::single();

        MigrationDriver::new().migrate(&src, &dest);
        assert_eq!(dest.data(), 2);
    }

    #[test]
    fn migrate_multi() {
        let src = TestDevice::multi();
        src.set_data(2);

        let dest = TestDevice::multi();

        MigrationDriver::new().migrate(&src, &dest);
        assert_eq!(dest.data(), 2);
    }

    #[test]
    fn migrate_empty() {
        let src = TestDevice::empty();
        src.set_data(2);

        let dest = TestDevice::empty();

        MigrationDriver::new().migrate(&src, &dest);
        assert_eq!(dest.data(), 0);
    }

    #[test]
    fn migrate_with_mem() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let src = TestDevice::multi();
        src.set_data(3);

        let dest = TestDevice::multi();

        MigrationDriver::with_mem(&mem).migrate(&src, &dest);

        assert_eq!(dest.mem(), 2);
        assert_eq!(dest.data(), 3);
    }

    #[test]
    fn migrate_with_ctx() {
        let mem = test_phys_map().finalize();
        mem.access().unwrap().write::<u8>(TEST_MEM_ADDR, &2);

        let src = TestDevice::multi();
        src.set_data(3);

        let dest = TestDevice::multi();

        let ctx = MigrateCtx { mem: &mem.access().unwrap() };
        migrate(&src, &dest, &ctx);

        assert_eq!(dest.mem(), 2);
        assert_eq!(dest.data(), 3);
    }

    #[test]
    #[should_panic = "test-device is not migratable"]
    fn panic_export_non_migratable() {
        let src = TestDevice::non_migratable();
        MigrationDriver::new().export(&src);
    }

    #[test]
    #[should_panic = "test-device is not migratable"]
    fn panic_import_non_migratable() {
        let src = TestDevice::single();
        let dest = TestDevice::non_migratable();
        MigrationDriver::new().migrate(&src, &dest);
    }

    #[test]
    #[should_panic = "expected single payload"]
    fn panic_import_empty() {
        let src = TestDevice::empty();
        let dest = TestDevice::single();
        MigrationDriver::new().migrate(&src, &dest);
    }

    #[test]
    #[should_panic = "expected multi payload"]
    fn panic_import_wrong_payload() {
        let src = TestDevice::single();
        let dest = TestDevice::multi();
        MigrationDriver::new().migrate(&src, &dest);
    }

    #[test]
    #[should_panic = "offers remaining after migration"]
    fn panic_skip_offer() {
        let src = TestDevice::multi();
        let dest = TestDevice::multi().skip_offer();
        MigrationDriver::new().migrate(&src, &dest);
    }
}
