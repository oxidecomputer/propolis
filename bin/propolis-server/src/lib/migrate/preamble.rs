use propolis_client::instance_spec::{
    BackendNames, DeviceSpec, InstanceSpec, MigrationCompatibilityError,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub device_spec: DeviceSpec,
    pub backend_keys: BackendNames,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance_spec: InstanceSpec) -> Preamble {
        Preamble {
            device_spec: instance_spec.devices.clone(),
            backend_keys: BackendNames::from(&instance_spec.backends),
            blobs: Vec::new(),
        }
    }

    pub fn is_migration_compatible(
        &self,
        other_spec: &InstanceSpec,
    ) -> Result<(), MigrationCompatibilityError> {
        self.device_spec.can_migrate_devices_from(&other_spec.devices)?;
        let other_keys = BackendNames::from(&other_spec.backends);
        self.backend_keys.can_migrate_backends_from(&other_keys)?;

        // TODO: Compare opaque blobs.

        Ok(())
    }
}
