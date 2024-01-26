// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines for starting VMs, changing their states, and interacting with their
//! guest OSes.

use std::{fmt::Debug, io::Write, sync::Arc, time::Duration};

use crate::{
    guest_os::{self, CommandSequenceEntry, GuestOs, GuestOsKind},
    serial::{BufferKind, SerialConsole},
    test_vm::{
        environment::Environment, server::ServerProcessParameters, spec::VmSpec,
    },
    Framework,
};

use anyhow::{anyhow, Context, Result};
use camino::Utf8PathBuf;
use core::result::Result as StdResult;
use propolis_client::{
    support::{InstanceSerialConsoleHelper, WSClientOffset},
    types::{
        InstanceGetResponse, InstanceMigrateInitiateRequest,
        InstanceProperties, InstanceSerialConsoleHistoryResponse,
        InstanceSpecEnsureRequest, InstanceSpecGetResponse, InstanceState,
        InstanceStateRequested, MigrationState, VersionedInstanceSpec,
    },
};
use propolis_client::{Client, ResponseValue};
use thiserror::Error;
use tokio::{sync::oneshot, time::timeout};
use tracing::{debug, error, info, info_span, instrument, warn, Instrument};
use uuid::Uuid;

type PropolisClientError =
    propolis_client::Error<propolis_client::types::Error>;
type PropolisClientResult<T> = StdResult<ResponseValue<T>, PropolisClientError>;

pub(crate) mod config;
pub(crate) mod environment;
mod server;
pub(crate) mod spec;

pub use config::*;
pub use environment::VmLocation;

use self::environment::EnvironmentSpec;

#[derive(Debug, Error)]
pub enum VmStateError {
    #[error("Operation can only be performed on a VM that has been ensured")]
    InstanceNotEnsured,

    #[error(
        "Operation can only be performed on a new VM that has not been ensured"
    )]
    InstanceAlreadyEnsured,
}

/// Specifies the timeout to apply to an attempt to migrate.
pub enum MigrationTimeout {
    /// Time out after the specified duration.
    Explicit(std::time::Duration),

    /// Allow MIGRATION_SECS_PER_GUEST_GIB seconds per GiB of guest memory.
    InferFromMemorySize,
}

/// Specifies the mechanism a new VM should use to obtain a serial console.
enum InstanceConsoleSource<'a> {
    /// Connect a new console to the VM's server's serial console endpoint.
    New,

    // Clone an existing console connection from the supplied VM.
    InheritFrom(&'a TestVm),
}

/// The number of seconds to add to the migration timeout per GiB of memory in
/// the migrating VM.
const MIGRATION_SECS_PER_GUEST_GIB: u64 = 90;

impl Default for MigrationTimeout {
    fn default() -> Self {
        Self::InferFromMemorySize
    }
}

enum VmState {
    New,
    Ensured { serial: SerialConsole },
}

/// A virtual machine running in a Propolis server. Test cases create these VMs
/// using the [`factory::VmFactory`] embedded in their test contexts.
///
/// Once a VM has been created, tests will usually want to issue [`TestVm::run`]
/// and [`TestVm::wait_to_boot`] calls so they can begin interacting with the
/// serial console.
pub struct TestVm {
    id: Uuid,
    rt: tokio::runtime::Handle,
    client: Client,
    server: server::PropolisServer,
    spec: VmSpec,
    environment_spec: EnvironmentSpec,
    data_dir: Utf8PathBuf,

    guest_os: Box<dyn GuestOs>,
    tracing_span: tracing::Span,

    state: VmState,
}

impl TestVm {
    /// Creates a new Propolis server, attaches a client to it, and issues an
    /// `instance_ensure` request to initialize the instance in the server, but
    /// does not actually run the instance.
    ///
    /// # Arguments
    ///
    /// - vm_name: A logical name to use to refer to this VM elsewhere in the
    ///   test harness.
    /// - process_params: The parameters to use to launch the server binary.
    /// - vm_config: The VM configuration (CPUs, memory, disks, etc.) the VM
    ///   will use.
    ///
    ///   Note that this routine currently only propagates the CPU and memory
    ///   configuration into the `instance_ensure` call. Device configuration
    ///   comes from the configuration TOML in the process parameters. The
    ///   caller is responsible for ensuring the correct config file lives in
    ///   this location.
    /// - guest_os_kind: The kind of guest OS this VM will host.
    #[instrument(skip_all)]
    pub(crate) fn new(
        framework: &Framework,
        spec: VmSpec,
        environment: &EnvironmentSpec,
    ) -> Result<Self> {
        let id = Uuid::new_v4();
        let guest_os_kind = spec.guest_os_kind;

        let vm_name = &spec.vm_name;
        info!(%vm_name, ?spec.instance_spec, ?guest_os_kind, ?environment);

        match environment
            .build(framework)
            .context("building environment for new VM")?
        {
            Environment::Local(params) => Self::start_local_vm(
                id,
                framework.tokio_rt.handle().clone(),
                spec,
                environment.clone(),
                params,
            ),
        }
    }

    fn start_local_vm(
        vm_id: Uuid,
        rt: tokio::runtime::Handle,
        vm_spec: VmSpec,
        environment_spec: EnvironmentSpec,
        params: ServerProcessParameters,
    ) -> Result<Self> {
        let config_filename = format!("{}.config.toml", &vm_spec.vm_name);
        let mut config_toml_path = params.data_dir.to_path_buf();
        config_toml_path.push(config_filename);
        let mut config_file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&config_toml_path)
            .with_context(|| {
                format!("opening config file {} for writing", config_toml_path)
            })?;

        config_file
            .write_all(vm_spec.config_toml_contents.as_bytes())
            .with_context(|| {
                format!(
                    "writing config toml to config file {}",
                    config_toml_path
                )
            })?;

        let span =
            info_span!(parent: None, "VM", vm = %vm_spec.vm_name, %vm_id);

        let data_dir = params.data_dir.to_path_buf();
        let server_addr = params.server_addr;
        let server = server::PropolisServer::new(
            &vm_spec.vm_name,
            params,
            &config_toml_path,
        )?;

        let client = Client::new(&format!("http://{}", server_addr));
        let guest_os = guest_os::get_guest_os_adapter(vm_spec.guest_os_kind);
        Ok(Self {
            id: vm_id,
            rt,
            client,
            server,
            spec: vm_spec,
            environment_spec,
            data_dir,
            guest_os,
            tracing_span: span,
            state: VmState::New,
        })
    }

    pub fn name(&self) -> &str {
        &self.spec.vm_name
    }

    pub fn cloned_disk_handles(&self) -> Vec<Arc<dyn crate::disk::DiskConfig>> {
        self.spec.disk_handles.clone()
    }

    pub(crate) fn vm_spec(&self) -> VmSpec {
        self.spec.clone()
    }

    pub(crate) fn environment_spec(&self) -> EnvironmentSpec {
        self.environment_spec.clone()
    }

    /// Sends an instance ensure request to this VM's server, allowing it to
    /// transition into the running state.
    async fn instance_ensure_async<'a>(
        &self,
        migrate: Option<InstanceMigrateInitiateRequest>,
        console_source: InstanceConsoleSource<'a>,
    ) -> Result<SerialConsole> {
        let _span = self.tracing_span.enter();
        let (vcpus, memory_mib) = match self.state {
            VmState::New => (
                self.spec.instance_spec.devices.board.cpus,
                self.spec.instance_spec.devices.board.memory_mb,
            ),
            VmState::Ensured { .. } => {
                return Err(VmStateError::InstanceAlreadyEnsured.into())
            }
        };

        let properties = InstanceProperties {
            id: self.id,
            name: format!("phd-vm-{}", self.id),
            description: "Pheidippides-managed VM".to_string(),
            image_id: Uuid::default(),
            bootrom_id: Uuid::default(),
            memory: memory_mib,
            vcpus,
        };

        let versioned_spec =
            VersionedInstanceSpec::V0(self.spec.instance_spec.clone());
        let ensure_req = InstanceSpecEnsureRequest {
            properties,
            instance_spec: versioned_spec,
            migrate,
        };

        // There is a brief period where the Propolis server process has begun
        // to run but hasn't started its Dropshot server yet. Ensure requests
        // that land in that window will fail, so retry them. This shouldn't
        // ever take more than a couple of seconds (if it does, that should be
        // considered a bug impacting VM startup times).
        let ensure_fn = || async {
            if let Err(e) = self
                .client
                .instance_spec_ensure()
                .body(&ensure_req)
                .send()
                .await
            {
                match e {
                    propolis_client::Error::CommunicationError(_) => {
                        info!(%e, "retriable error from instance_spec_ensure");
                        Err(backoff::Error::transient(e))
                    }
                    _ => {
                        error!(%e, "permanent error from instance_spec_ensure");
                        Err(backoff::Error::permanent(e))
                    }
                }
            } else {
                Ok(())
            }
        };

        backoff::future::retry(
            backoff::ExponentialBackoff {
                max_elapsed_time: Some(std::time::Duration::from_secs(2)),
                ..Default::default()
            },
            ensure_fn,
        )
        .await?;

        let helper = InstanceSerialConsoleHelper::new(
            std::net::SocketAddr::V4(self.server.server_addr()),
            WSClientOffset::MostRecent(0),
            None,
        )
        .await?;

        let console = match console_source {
            InstanceConsoleSource::New => {
                SerialConsole::new(
                    helper,
                    BufferKind::Raw,
                    self.serial_log_file_path(),
                )
                .await?
            }
            InstanceConsoleSource::InheritFrom(vm) => match &vm.state {
                VmState::New => anyhow::bail!(
                    "tried to inherit console from an unstarted VM"
                ),
                VmState::Ensured { serial } => (*serial).clone(),
            },
        };

        let instance_description =
            self.client.instance_get().send().await.with_context(|| {
                anyhow!("failed to get instance properties")
            })?;

        info!(
            ?instance_description.instance,
            ?self.spec.instance_spec,
            "Started instance"
        );

        Ok(console)
    }

    /// Returns the kind of guest OS running in this VM.
    pub fn guest_os_kind(&self) -> GuestOsKind {
        self.spec.guest_os_kind
    }

    /// Sets the VM to the running state. If the VM has not yet been launched
    /// (by sending a Propolis instance-ensure request to it), send that request
    /// first.
    pub fn launch(&mut self) -> Result<()> {
        self.instance_ensure()?;
        self.run()?;
        Ok(())
    }

    /// Sends an instance ensure request to this VM's server, but does not run
    /// the VM.
    pub fn instance_ensure(&mut self) -> Result<()> {
        match self.state {
            VmState::New => {
                let console = self.rt.block_on(async {
                    self.instance_ensure_async(None, InstanceConsoleSource::New)
                        .await
                })?;
                self.state = VmState::Ensured { serial: console };
            }
            VmState::Ensured { .. } => {}
        }

        Ok(())
    }

    /// Sets the VM to the running state without first sending an instance
    /// ensure request.
    pub fn run(&self) -> PropolisClientResult<()> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Run).await
        })
    }

    /// Stops the VM.
    pub fn stop(&self) -> PropolisClientResult<()> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Stop).await
        })
    }

    /// Resets the VM by requesting the `Reboot` state from the server (as
    /// distinct from requesting a reboot from within the guest).
    pub fn reset(&self) -> PropolisClientResult<()> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Reboot).await
        })
    }

    async fn put_instance_state_async(
        &self,
        state: InstanceStateRequested,
    ) -> PropolisClientResult<()> {
        let _span = self.tracing_span.enter();
        info!(?state, "Requesting instance state change");
        self.client.instance_state_put().body(state).send().await
    }

    /// Issues a Propolis client `instance_get` request.
    pub fn get(&self) -> Result<InstanceGetResponse> {
        let _span = self.tracing_span.enter();
        info!("Sending instance get request to server");
        self.rt.block_on(async { self.get_async().await })
    }

    async fn get_async(&self) -> Result<InstanceGetResponse> {
        self.client
            .instance_get()
            .send()
            .await
            .map(ResponseValue::into_inner)
            .with_context(|| anyhow!("failed to query instance properties"))
    }

    pub fn get_spec(&self) -> Result<InstanceSpecGetResponse> {
        let _span = self.tracing_span.enter();
        info!("Sending instance spec get request to server");
        self.rt.block_on(async { self.get_spec_async().await })
    }

    async fn get_spec_async(&self) -> Result<InstanceSpecGetResponse> {
        self.client
            .instance_spec_get()
            .send()
            .await
            .map(ResponseValue::into_inner)
            .with_context(|| anyhow!("failed to query instance spec"))
    }

    /// Starts this instance by issuing an ensure request that specifies a
    /// migration from `source` and then running the target.
    pub fn migrate_from(
        &mut self,
        source: &Self,
        migration_id: Uuid,
        timeout: MigrationTimeout,
    ) -> Result<()> {
        let timeout_duration = match timeout {
            MigrationTimeout::Explicit(val) => val,
            MigrationTimeout::InferFromMemorySize => {
                let mem_mib = self.spec.instance_spec.devices.board.memory_mb;
                std::time::Duration::from_secs(
                    (MIGRATION_SECS_PER_GUEST_GIB * mem_mib) / 1024,
                )
            }
        };

        let _vm_guard = self.tracing_span.enter();
        match self.state {
            VmState::New => {
                info!(
                    ?migration_id,
                    ?timeout_duration,
                    "Migrating from source at address {}",
                    source.server.server_addr()
                );

                let serial = self.rt.block_on(async {
                    self.instance_ensure_async(
                        Some(InstanceMigrateInitiateRequest {
                            migration_id,
                            src_addr: source.server.server_addr().to_string(),
                            src_uuid: Uuid::default(),
                        }),
                        InstanceConsoleSource::InheritFrom(source),
                    )
                    .await
                })?;

                self.state = VmState::Ensured { serial };

                let span = info_span!("migrate", ?migration_id);
                let _guard = span.enter();
                let watch_migrate =
                    || -> Result<(), backoff::Error<anyhow::Error>> {
                        let state = self
                            .get_migration_state(migration_id)
                            .map_err(backoff::Error::Permanent)?;
                        match state {
                            MigrationState::Finish => {
                                info!("Migration completed successfully");
                                Ok(())
                            }
                            MigrationState::Error => {
                                info!(
                                    "Instance reported error during migration"
                                );
                                Err(backoff::Error::Permanent(anyhow!(
                                    "error during migration"
                                )))
                            }
                            _ => Err(backoff::Error::Transient {
                                err: anyhow!("migration not done yet"),
                                retry_after: None,
                            }),
                        }
                    };

                let backoff = backoff::ExponentialBackoff {
                    max_elapsed_time: Some(timeout_duration),
                    ..Default::default()
                };
                backoff::retry(backoff, watch_migrate)
                    .map_err(|e| anyhow!("error during migration: {}", e))
            }
            VmState::Ensured { .. } => {
                Err(VmStateError::InstanceAlreadyEnsured.into())
            }
        }
    }

    pub fn get_migration_state(
        &self,
        migration_id: Uuid,
    ) -> Result<MigrationState> {
        self.rt.block_on(async {
            Ok(self
                .client
                .instance_migrate_status()
                .migration_id(migration_id)
                .send()
                .await?
                .state)
        })
    }

    pub fn get_serial_console_history(
        &self,
        from_start: u64,
    ) -> Result<InstanceSerialConsoleHistoryResponse> {
        self.rt.block_on(async {
            Ok(self
                .client
                .instance_serial_history_get()
                .from_start(from_start)
                .send()
                .await?
                .into_inner())
        })
    }

    pub fn wait_for_state(
        &self,
        target: InstanceState,
        timeout_duration: Duration,
    ) -> Result<()> {
        let _span = self.tracing_span.enter();
        info!(
            "Waiting {:?} for server to reach state {:?}",
            timeout_duration, target
        );

        let wait_fn = || -> Result<(), backoff::Error<anyhow::Error>> {
            let current =
                self.get().map_err(backoff::Error::Permanent)?.instance.state;
            if current == target {
                Ok(())
            } else {
                Err(backoff::Error::transient(anyhow!(
                    "not in desired state yet: current {:?}, target {:?}",
                    current,
                    target
                )))
            }
        };

        let backoff = backoff::ExponentialBackoff {
            max_elapsed_time: Some(timeout_duration),
            ..Default::default()
        };
        backoff::retry(backoff, wait_fn)
            .map_err(|e| anyhow!("error waiting for instance state: {}", e))
    }

    /// Waits for the guest to reach a login prompt and then logs in. Note that
    /// login is not automated: this call is required to get to a shell prompt
    /// to allow the use of [`Self::run_shell_command`].
    ///
    /// This routine consumes all of the serial console input that precedes the
    /// initial login prompt and the login prompt itself.
    pub fn wait_to_boot(&self) -> Result<()> {
        let timeout_duration = Duration::from_secs(300);
        let _span = self.tracing_span.enter();
        info!("Waiting {:?} for guest to boot", timeout_duration);

        let boot_sequence = self.guest_os.get_login_sequence();
        let _ = self.rt.block_on(async {
            timeout(
                timeout_duration,
                async move {
                    for step in boot_sequence.0 {
                        debug!(?step, "executing command in boot sequence");
                        match step {
                            CommandSequenceEntry::WaitFor(s) => {
                                self.wait_for_serial_output_async(
                                    s,
                                    Duration::MAX,
                                )
                                .await?;
                            }
                            CommandSequenceEntry::WriteStr(s) => {
                                self.send_serial_str_async(s).await?;
                                self.send_serial_str_async("\n").await?;
                            }
                            CommandSequenceEntry::ChangeSerialConsoleBuffer(
                                kind,
                            ) => {
                                self.change_serial_buffer_kind(kind)?;
                            }
                            CommandSequenceEntry::SetSerialByteWriteDelay(
                                duration,
                            ) => {
                                self.set_serial_byte_write_delay(duration)?;
                            }
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .instrument(info_span!("wait_to_boot")),
            )
            .await
            .map_err(|e| anyhow!(e))
        })?;

        info!("Guest has booted");
        Ok(())
    }

    /// Waits for up to `timeout_duration` for `line` to appear on the guest
    /// serial console, then returns the unconsumed portion of the serial
    /// console buffer that preceded the requested string.
    pub fn wait_for_serial_output(
        &self,
        line: &str,
        timeout_duration: std::time::Duration,
    ) -> Result<String> {
        let _span = self.tracing_span.enter();
        info!(
            target = line,
            ?timeout_duration,
            "Waiting for output on serial console"
        );

        let received: Option<String> = self.rt.block_on(async {
            self.wait_for_serial_output_async(line, timeout_duration).await
        })?;

        received.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Channel unexpectedly closed while waiting for string",
            )
            .into()
        })
    }

    async fn wait_for_serial_output_async(
        &self,
        line: &str,
        timeout_duration: Duration,
    ) -> Result<Option<String>> {
        let line = line.to_string();
        let (preceding_tx, preceding_rx) = oneshot::channel();
        match &self.state {
            VmState::Ensured { serial } => {
                serial.register_wait_for_string(line.clone(), preceding_tx)?;
                let t = timeout(timeout_duration, preceding_rx).await;
                match t {
                    Err(timeout_elapsed) => {
                        serial.cancel_wait_for_string()?;
                        Err(anyhow!(timeout_elapsed))
                    }
                    Ok(Err(e)) => Err(e.into()),
                    Ok(Ok(received_string)) => Ok(Some(received_string)),
                }
            }
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
    }

    /// Runs the shell command `cmd` by sending it to the serial console, then
    /// waits for another shell prompt to appear using
    /// [`Self::wait_for_serial_output`] and returns any text that was buffered
    /// to the serial console after the command was sent.
    pub fn run_shell_command(&self, cmd: &str) -> Result<String> {
        let amended = self.guest_os.amend_shell_command(cmd);
        let to_send = amended.as_deref().unwrap_or(cmd);

        // If the command is multi-line, it won't be echoed literally.
        // instead, it will (probably) have each line begin with an `>`. so,
        // fix that.
        let to_send = to_send.trim_end().replace('\n', "\n> ");
        self.send_serial_str(&to_send)?;

        // Consume the amended command from the buffer so that it doesn't show
        // up in the command output.
        self.wait_for_serial_output(&to_send, Duration::from_secs(15))?;
        self.send_serial_str("\n")?;

        let out = self.wait_for_serial_output(
            self.guest_os.get_shell_prompt(),
            Duration::from_secs(300),
        )?;

        // Trim both ends of the output to get rid of any echoed newlines and/or
        // whitespace that were inserted when sending '\n' to start processing
        // the command.
        Ok(out.trim().to_string())
    }

    fn send_serial_str(&self, string: &str) -> Result<()> {
        self.rt.block_on(async { self.send_serial_str_async(string).await })
    }

    async fn send_serial_str_async(&self, string: &str) -> Result<()> {
        if !string.is_empty() {
            self.send_serial_bytes_async(Vec::from(string.as_bytes())).await
        } else {
            Ok(())
        }
    }

    async fn send_serial_bytes_async(&self, bytes: Vec<u8>) -> Result<()> {
        match &self.state {
            VmState::Ensured { serial } => serial.send_bytes(bytes),
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
    }

    fn change_serial_buffer_kind(&self, kind: BufferKind) -> Result<()> {
        match &self.state {
            VmState::Ensured { serial } => serial.change_buffer_kind(kind),
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
    }

    fn set_serial_byte_write_delay(
        &self,
        delay: std::time::Duration,
    ) -> Result<()> {
        match &self.state {
            VmState::Ensured { serial } => serial.set_guest_write_delay(delay),
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
    }

    /// Indicates whether this VM's guest OS has a read-only filesystem.
    pub fn guest_os_has_read_only_fs(&self) -> bool {
        self.guest_os.read_only_fs()
    }

    /// Generates a path to a file into which the VM's serial console adapter
    /// can log serial console output.
    fn serial_log_file_path(&self) -> Utf8PathBuf {
        let filename = format!("{}.serial.log", self.spec.vm_name);
        let mut path = self.data_dir.clone();
        path.push(filename);
        path
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        let _span = self.tracing_span.enter();

        if let VmState::New = self.state {
            // Instance never ensured, nothing to do.
            return;
        }

        match self.get().map(|r| r.instance.state) {
            Ok(InstanceState::Destroyed) => {
                // Instance already destroyed, nothing to do.
            }
            Ok(_) => {
                // Instance is up, best-effort attempt to let it clean up gracefully.
                info!("Cleaning up Test VM on drop");
                if let Err(err) = self.stop() {
                    warn!(?err, "Stop request failed for Test VM cleanup");
                    return;
                }
                if let Err(err) = self.wait_for_state(
                    InstanceState::Destroyed,
                    Duration::from_secs(5),
                ) {
                    warn!(?err, "Test VM failed to clean up");
                }
            }
            Err(err) => {
                // Instance should've been ensured by this point so an error is unexpected.
                warn!(?err, "Unexpected error from instance");
            }
        }
    }
}
