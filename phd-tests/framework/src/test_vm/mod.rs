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
        InstanceMigrateStatusResponse, InstanceProperties,
        InstanceSerialConsoleHistoryResponse, InstanceSpecEnsureRequest,
        InstanceSpecGetResponse, InstanceState, InstanceStateRequested,
        MigrationState, VersionedInstanceSpec,
    },
};
use propolis_client::{Client, ResponseValue};
use thiserror::Error;
use tokio::{
    sync::{mpsc::UnboundedSender, oneshot},
    task::JoinHandle,
    time::timeout,
};
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

/// The number of seconds to add to the migration timeout per GiB of memory in
/// the migrating VM.
const MIGRATION_SECS_PER_GUEST_GIB: u64 = 90;

impl Default for MigrationTimeout {
    fn default() -> Self {
        Self::InferFromMemorySize
    }
}

impl From<std::time::Duration> for MigrationTimeout {
    fn from(value: std::time::Duration) -> Self {
        Self::Explicit(value)
    }
}

/// Specifies the timeout to apply when waiting for output to appear on the
/// serial console.
#[derive(Debug)]
pub enum SerialOutputTimeout {
    /// Time out after the specified duration.
    Explicit(std::time::Duration),

    /// The caller is waiting for the serial console as part of a larger
    /// operation with its own timeout, so don't set an explicit timeout on this
    /// wait.
    CallerTimeout,
}

impl From<std::time::Duration> for SerialOutputTimeout {
    fn from(value: std::time::Duration) -> Self {
        Self::Explicit(value)
    }
}

impl From<SerialOutputTimeout> for std::time::Duration {
    fn from(value: SerialOutputTimeout) -> Self {
        match value {
            SerialOutputTimeout::Explicit(t) => t,
            SerialOutputTimeout::CallerTimeout => Duration::MAX,
        }
    }
}

/// Specifies the mechanism a new VM should use to obtain a serial console.
enum InstanceConsoleSource<'a> {
    /// Connect a new console to the VM's server's serial console endpoint.
    New,

    // Clone an existing console connection from the supplied VM.
    InheritFrom(&'a TestVm),
}

enum VmState {
    New,
    Ensured { serial: SerialConsole },
}

/// A virtual machine running in a Propolis server. Test cases create these VMs
/// using the `factory::VmFactory` embedded in their test contexts.
///
/// Once a VM has been created, tests will usually want to issue [`TestVm::run`]
/// and [`TestVm::wait_to_boot`] calls so they can begin interacting with the
/// serial console.
pub struct TestVm {
    id: Uuid,
    client: Client,
    server: Option<server::PropolisServer>,
    spec: VmSpec,
    environment_spec: EnvironmentSpec,
    data_dir: Utf8PathBuf,

    guest_os: Box<dyn GuestOs>,

    state: VmState,

    /// Sending a task handle to this channel will ensure that the task runs to
    /// completion as part of the post-test cleanup fixture (i.e. before any
    /// other tests run).
    cleanup_task_tx: UnboundedSender<JoinHandle<()>>,
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
    pub(crate) async fn new(
        framework: &Framework,
        spec: VmSpec,
        environment: &EnvironmentSpec,
    ) -> Result<Self> {
        let id = Uuid::new_v4();
        let guest_os_kind = spec.guest_os_kind;

        let vm_name = &spec.vm_name;

        // TODO(#735): It would be nice to log the instance spec here too, but
        // this is extremely noisy for disks with an in-memory disk backend. The
        // problem is that this spec is a propolis-client generated type with a
        // derived Debug impl. This can be fixed by making propolis-client
        // re-export the instance spec types from propolis_api_types (instead of
        // generating them) so that it can pick up the latter crate's explicit
        // Debug impls for verbose component types.
        info!(%vm_name, ?guest_os_kind, ?environment);

        match environment
            .build(framework)
            .await
            .context("building environment for new VM")?
        {
            Environment::Local(params) => Self::start_local_vm(
                id,
                spec,
                environment.clone(),
                params,
                framework.cleanup_task_channel(),
            ),
        }
    }

    fn start_local_vm(
        vm_id: Uuid,
        vm_spec: VmSpec,
        environment_spec: EnvironmentSpec,
        params: ServerProcessParameters,
        cleanup_task_tx: UnboundedSender<JoinHandle<()>>,
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
            client,
            server: Some(server),
            spec: vm_spec,
            environment_spec,
            data_dir,
            guest_os,
            state: VmState::New,
            cleanup_task_tx,
        })
    }

    pub fn name(&self) -> &str {
        &self.spec.vm_name
    }

    pub fn cloned_disk_handles(&self) -> Vec<Arc<dyn crate::disk::DiskConfig>> {
        self.spec.disk_handles.clone()
    }

    pub fn vm_spec(&self) -> VmSpec {
        self.spec.clone()
    }

    pub fn environment_spec(&self) -> EnvironmentSpec {
        self.environment_spec.clone()
    }

    /// Sends an instance ensure request to this VM's server, allowing it to
    /// transition into the running state.
    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    async fn instance_ensure_internal<'a>(
        &self,
        migrate: Option<InstanceMigrateInitiateRequest>,
        console_source: InstanceConsoleSource<'a>,
    ) -> Result<SerialConsole> {
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
            metadata: self.spec.metadata.clone(),
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
            std::net::SocketAddr::V4(
                self.server
                    .as_ref()
                    .expect("server should be alive")
                    .server_addr(),
            ),
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
    pub async fn launch(&mut self) -> Result<()> {
        self.instance_ensure().await?;
        self.run().await?;
        Ok(())
    }

    /// Sends an instance ensure request to this VM's server, but does not run
    /// the VM.
    pub async fn instance_ensure(&mut self) -> Result<()> {
        match self.state {
            VmState::New => {
                let console = self
                    .instance_ensure_internal(None, InstanceConsoleSource::New)
                    .await?;
                self.state = VmState::Ensured { serial: console };
            }
            VmState::Ensured { .. } => {}
        }

        Ok(())
    }

    /// Sets the VM to the running state without first sending an instance
    /// ensure request.
    pub async fn run(&self) -> PropolisClientResult<()> {
        self.put_instance_state(InstanceStateRequested::Run).await
    }

    /// Stops the VM.
    pub async fn stop(&self) -> PropolisClientResult<()> {
        self.put_instance_state(InstanceStateRequested::Stop).await
    }

    /// Resets the VM by requesting the `Reboot` state from the server (as
    /// distinct from requesting a reboot from within the guest).
    pub async fn reset(&self) -> PropolisClientResult<()> {
        self.put_instance_state(InstanceStateRequested::Reboot).await
    }

    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    async fn put_instance_state(
        &self,
        state: InstanceStateRequested,
    ) -> PropolisClientResult<()> {
        info!(?state, "Requesting instance state change");
        self.client.instance_state_put().body(state).send().await
    }

    /// Issues a Propolis client `instance_get` request.
    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    pub async fn get(&self) -> Result<InstanceGetResponse> {
        info!("Sending instance get request to server");
        self.client
            .instance_get()
            .send()
            .await
            .map(ResponseValue::into_inner)
            .with_context(|| anyhow!("failed to query instance properties"))
    }

    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    pub async fn get_spec(&self) -> Result<InstanceSpecGetResponse> {
        info!("Sending instance spec get request to server");
        self.client
            .instance_spec_get()
            .send()
            .await
            .map(ResponseValue::into_inner)
            .with_context(|| anyhow!("failed to query instance spec"))
    }

    /// Starts this instance by issuing an ensure request that specifies a
    /// migration from `source` and then running the target.
    #[instrument(
        skip_all,
        fields(
            source = source.spec.vm_name,
            target = self.spec.vm_name,
            source_id = %source.id,
            target_id = %self.id
        )
    )]
    pub async fn migrate_from(
        &mut self,
        source: &Self,
        migration_id: Uuid,
        timeout: impl Into<MigrationTimeout>,
    ) -> Result<()> {
        let timeout_duration = match Into::<MigrationTimeout>::into(timeout) {
            MigrationTimeout::Explicit(val) => val,
            MigrationTimeout::InferFromMemorySize => {
                let mem_mib = self.spec.instance_spec.devices.board.memory_mb;
                std::time::Duration::from_secs(
                    (MIGRATION_SECS_PER_GUEST_GIB * mem_mib) / 1024,
                )
            }
        };

        match self.state {
            VmState::New => {
                let server_addr = source
                    .server
                    .as_ref()
                    .expect("source server should be alive")
                    .server_addr();

                info!(
                    ?migration_id,
                    ?timeout_duration,
                    "Migrating from source at address {}",
                    server_addr
                );

                let serial = self
                    .instance_ensure_internal(
                        Some(InstanceMigrateInitiateRequest {
                            migration_id,
                            src_addr: server_addr.to_string(),
                            src_uuid: Uuid::default(),
                        }),
                        InstanceConsoleSource::InheritFrom(source),
                    )
                    .await?;

                self.state = VmState::Ensured { serial };

                let span = info_span!("migrate", ?migration_id);
                let _guard = span.enter();
                let migrate_fn = || async {
                    let state = self
                        .get_migration_state()
                        .await
                        .map_err(backoff::Error::Permanent)?
                        .migration_in
                        .expect("instance should be migrating in")
                        .state;

                    match state {
                        MigrationState::Finish => {
                            info!("Migration completed successfully");
                            Ok(())
                        }
                        MigrationState::Error => {
                            info!("Instance reported error during migration");
                            Err(backoff::Error::Permanent(anyhow!(
                                "error during migration"
                            )))
                        }
                        _ => Err(backoff::Error::transient(anyhow!(
                            "migration not done yet"
                        ))),
                    }
                };

                backoff::future::retry(
                    backoff::ExponentialBackoff {
                        max_elapsed_time: Some(timeout_duration),
                        ..Default::default()
                    },
                    migrate_fn,
                )
                .await
                .context("live migration")?;

                Ok(())
            }
            VmState::Ensured { .. } => {
                Err(VmStateError::InstanceAlreadyEnsured.into())
            }
        }
    }

    pub async fn get_migration_state(
        &self,
    ) -> Result<InstanceMigrateStatusResponse> {
        Ok(self.client.instance_migrate_status().send().await?.into_inner())
    }

    pub async fn get_serial_console_history(
        &self,
        from_start: u64,
    ) -> Result<InstanceSerialConsoleHistoryResponse> {
        Ok(self
            .client
            .instance_serial_history_get()
            .from_start(from_start)
            .send()
            .await?
            .into_inner())
    }

    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    pub async fn wait_for_state(
        &self,
        target: InstanceState,
        timeout_duration: Duration,
    ) -> Result<()> {
        info!(
            "Waiting {:?} for server to reach state {:?}",
            timeout_duration, target
        );

        let wait_fn = || async {
            let current = self
                .get()
                .await
                .map_err(backoff::Error::Permanent)?
                .instance
                .state;

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

        backoff::future::retry(
            backoff::ExponentialBackoff {
                max_elapsed_time: Some(timeout_duration),
                ..Default::default()
            },
            wait_fn,
        )
        .await
        .context("waiting for instance state")?;

        Ok(())
    }

    /// Waits for the guest to reach a login prompt and then logs in. Note that
    /// login is not automated: this call is required to get to a shell prompt
    /// to allow the use of [`Self::run_shell_command`].
    ///
    /// This routine consumes all of the serial console input that precedes the
    /// initial login prompt and the login prompt itself.
    pub async fn wait_to_boot(&self) -> Result<()> {
        let timeout_duration = Duration::from_secs(300);
        let boot_sequence = self.guest_os.get_login_sequence();
        let boot = async move {
            info!(
                vm = self.spec.vm_name,
                vm_id = %self.id,
                ?timeout_duration,
                "waiting for guest to boot"
            );

            for step in boot_sequence.0 {
                debug!(?step, "executing command in boot sequence");
                match step {
                    CommandSequenceEntry::WaitFor(s) => {
                        self.wait_for_serial_output(
                            s.as_ref(),
                            SerialOutputTimeout::CallerTimeout,
                        )
                        .await?;
                    }
                    CommandSequenceEntry::WriteStr(s) => {
                        self.send_serial_str(s.as_ref()).await?;
                        self.send_serial_str("\n").await?;
                    }
                    CommandSequenceEntry::EstablishConsistentEcho {
                        send,
                        expect,
                        timeout,
                    } => {
                        self.establish_serial_console_echo(
                            send.as_ref(),
                            expect.as_ref(),
                            timeout,
                            SerialOutputTimeout::CallerTimeout,
                        )
                        .await?;
                    }
                    CommandSequenceEntry::ClearBuffer => {
                        self.clear_serial_buffer()?
                    }
                    CommandSequenceEntry::ChangeSerialConsoleBuffer(kind) => {
                        self.change_serial_buffer_kind(kind)?;
                    }
                    CommandSequenceEntry::SetRepeatedCharacterDebounce(
                        duration,
                    ) => {
                        self.set_serial_repeated_character_debounce(duration)?;
                    }
                }
            }

            info!("Guest has booted");
            Ok::<(), anyhow::Error>(())
        }
        .instrument(info_span!("wait_to_boot"));

        match timeout(timeout_duration, boot).await {
            Err(_) => anyhow::bail!("timed out while waiting to boot"),
            Ok(inner) => {
                inner.context("executing guest login sequence")?;
            }
        };

        Ok(())
    }

    /// Waits for up to `timeout_duration` for `line` to appear on the guest
    /// serial console, then returns the contents of the console buffer that
    /// preceded the requested string.
    #[instrument(skip_all, fields(vm = self.spec.vm_name, vm_id = %self.id))]
    pub async fn wait_for_serial_output(
        &self,
        line: &str,
        timeout_duration: impl Into<SerialOutputTimeout>,
    ) -> Result<String> {
        let timeout_duration: SerialOutputTimeout = timeout_duration.into();
        info!(
            target = line,
            ?timeout_duration,
            "Waiting for output on serial console"
        );

        let received = {
            let line = line.to_string();
            let (preceding_tx, preceding_rx) = oneshot::channel();
            match &self.state {
                VmState::Ensured { serial } => {
                    serial
                        .register_wait_for_string(line.clone(), preceding_tx)?;
                    let t =
                        timeout(timeout_duration.into(), preceding_rx).await;
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
        };

        received?.ok_or_else(|| {
            anyhow!("wait_for_serial_output recv channel unexpectedly closed")
        })
    }

    /// Attempts to establish that the guest serial console consistently echoes
    /// characters by writing `send` and waiting for `expect` to appear within
    /// the supplied `timeout`.
    ///
    /// This function will back off between attempts to send and await
    /// characters (but will *not* change the delay used to wait for characters
    /// to be echoed) and will retry for up to the duration specified by
    /// `overall_timeout`.
    async fn establish_serial_console_echo(
        &self,
        send: &str,
        expect: &str,
        expect_timeout: std::time::Duration,
        overall_timeout: impl Into<SerialOutputTimeout>,
    ) -> Result<()> {
        let overall_timeout: SerialOutputTimeout = overall_timeout.into();
        info!(
            send,
            expect,
            ?expect_timeout,
            ?overall_timeout,
            "establishing serial console echo"
        );

        let send_and_expect = || async {
            self.send_serial_str(send).await?;
            self.wait_for_serial_output(expect, expect_timeout)
                .await
                .map(|_| ())
                .map_err(backoff::Error::transient)
        };

        backoff::future::retry(
            backoff::ExponentialBackoff {
                max_elapsed_time: match overall_timeout {
                    SerialOutputTimeout::Explicit(d) => Some(d),
                    SerialOutputTimeout::CallerTimeout => None,
                },
                ..Default::default()
            },
            send_and_expect,
        )
        .await?;

        Ok(())
    }

    /// Runs the shell command `cmd` by sending it to the serial console, then
    /// waits for another shell prompt to appear using
    /// [`Self::wait_for_serial_output`] and returns any text that was buffered
    /// to the serial console after the command was sent.
    pub async fn run_shell_command(&self, cmd: &str) -> Result<String> {
        // Allow the guest OS to transform the input command into a
        // guest-specific command sequence. This accounts for the guest's shell
        // type (which affects e.g. affects how it displays multi-line commands)
        // and serial console buffering discipline.
        let command_sequence = self.guest_os.shell_command_sequence(cmd);
        for step in command_sequence.0 {
            match step {
                CommandSequenceEntry::WaitFor(s) => {
                    self.wait_for_serial_output(
                        s.as_ref(),
                        std::time::Duration::from_secs(15),
                    )
                    .await?;
                }
                CommandSequenceEntry::WriteStr(s) => {
                    self.send_serial_str(s.as_ref()).await?;
                }
                CommandSequenceEntry::ClearBuffer => {
                    self.clear_serial_buffer()?
                }
                _ => {
                    anyhow::bail!(
                        "Unexpected command sequence entry {step:?} while \
                        running shell command"
                    );
                }
            }
        }

        // `shell_command_sequence` promises that the generated command sequence
        // clears buffer of everything up to and including the input command
        // before actually issuing the final '\n' that issues the command.
        // This ensures that the buffer contents returned by this call contain
        // only the command's output.
        let out = self
            .wait_for_serial_output(
                self.guest_os.get_shell_prompt(),
                Duration::from_secs(300),
            )
            .await?;

        // Trim any leading newlines inserted when the command was issued and
        // any trailing whitespace that isn't actually part of the command
        // output. Any other embedded whitespace is the caller's problem.
        Ok(out.trim().to_string())
    }

    /// Sends `string` to the guest's serial console worker, then waits for the
    /// entire string to be sent to the guest before returning.
    pub async fn send_serial_str(&self, string: &str) -> Result<()> {
        if !string.is_empty() {
            self.send_serial_bytes(Vec::from(string.as_bytes()))?.await?;
        }
        Ok(())
    }

    fn serial_console(&self) -> Result<&SerialConsole> {
        match &self.state {
            VmState::Ensured { serial } => Ok(serial),
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
    }

    fn send_serial_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> Result<oneshot::Receiver<()>> {
        self.serial_console()?.send_bytes(bytes)
    }

    fn clear_serial_buffer(&self) -> Result<()> {
        self.serial_console()?.clear()
    }

    fn change_serial_buffer_kind(&self, kind: BufferKind) -> Result<()> {
        self.serial_console()?.change_buffer_kind(kind)
    }

    fn set_serial_repeated_character_debounce(
        &self,
        delay: std::time::Duration,
    ) -> Result<()> {
        self.serial_console()?.set_repeated_character_debounce(delay)
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
        if let VmState::New = self.state {
            return;
        }

        // Propolis processes don't automatically release their bhyve VMMs on
        // process shutdown--this has to be done explicitly by stopping the VM.
        // Unfortunately, the Propolis client is fully asynchronous, and because
        // VMs might get dropped from an async context, it's not possible to use
        // `block_on` here to guarantee that VMMs are synchronously cleaned up
        // when a `TestVm` is dropped.
        //
        // To drop the VMM safely, destructure this VM into its client, server,
        // and attached disk objects, and hand them all off to a separate
        // destructor task. Once the task is spawned, send it back to the
        // framework so that the test runner can wait for all the VMs destroyed
        // by a test case to be reaped before starting another test.
        let client = self.client.clone();
        let mut server = self.server.take().expect(
            "TestVm should always have a valid server until it's dropped",
        );

        let disks: Vec<_> = self.vm_spec().disk_handles.drain(..).collect();

        // The order in which the task destroys objects is important: the server
        // can't be killed until the client has gotten a chance to shut down
        // the VM, and the disks can't be destroyed until the server process has
        // been killed.
        let task = tokio::spawn(
            async move {
                // The task doesn't use the disks directly, but they need to be
                // kept alive until the server process is gone.
                let _disks = disks;

                // Try to make sure the server's kernel VMM is cleaned up before
                // killing the server process. This is best-effort; if it fails,
                // the kernel VMM is leaked. This generally indicates a bug in
                // Propolis (e.g. a VMM reference leak or an instance taking an
                // unexpectedly long time to stop).
                try_ensure_vm_destroyed(&client).await;

                // Make sure the server process is dead before trying to clean
                // up any disks. Otherwise, ZFS may refuse to delete a cloned
                // disk because the server process still has it open.
                server.kill();
            }
            .instrument(
                info_span!("VM cleanup", vm = self.spec.vm_name, vm_id = %self.id),
            ),
        );

        let _ = self.cleanup_task_tx.send(task);
    }
}

/// Attempts to ensure that the Propolis server referred to by `client` is in
/// the `Destroyed` state by stopping any VM that happens to be running in that
/// server.
///
/// This function is best-effort.
async fn try_ensure_vm_destroyed(client: &Client) {
    match client.instance_get().send().await.map(|r| r.instance.state) {
        Ok(InstanceState::Destroyed) => return,
        Err(error) => warn!(
            %error,
            "error getting instance state from dropped VM"
        ),
        Ok(_) => {}
    }

    debug!("trying to ensure Propolis server VM is destroyed");
    if let Err(error) = client
        .instance_state_put()
        .body(InstanceStateRequested::Stop)
        .send()
        .await
    {
        // If the put fails because the instance was already run down, there's
        // nothing else to do. If it fails for some other reason, there's
        // nothing else that *can* be done, but the error is unusual and should
        // be logged.
        match error.status() {
            Some(http::status::StatusCode::FAILED_DEPENDENCY) => {}
            _ => {
                error!(
                    %error,
                    "error stopping VM to move it to Destroyed"
                );
            }
        }

        return;
    }

    let check_destroyed = || async {
        match client.instance_get().send().await.map(|r| r.instance.state) {
            Ok(InstanceState::Destroyed) => Ok(()),
            Ok(state) => Err(backoff::Error::transient(anyhow::anyhow!(
                "instance not destroyed yet (state: {:?})",
                state
            ))),
            Err(error) => {
                error!(
                    %error,
                    "failed to get state of VM being destroyed"
                );
                Err(backoff::Error::permanent(error.into()))
            }
        }
    };

    let destroyed = backoff::future::retry(
        backoff::ExponentialBackoff {
            max_elapsed_time: Some(std::time::Duration::from_secs(5)),
            ..Default::default()
        },
        check_destroyed,
    )
    .await;

    if let Err(error) = destroyed {
        error!(%error, "VM not destroyed after 5 seconds");
    }
}
