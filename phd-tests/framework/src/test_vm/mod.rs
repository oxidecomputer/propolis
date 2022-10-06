//! Routines for starting VMs, changing their states, and interacting with their
//! guest OSes.

use std::{fmt::Debug, process::Stdio, time::Duration};

use crate::guest_os::{self, CommandSequenceEntry, GuestOs, GuestOsKind};
use crate::serial::SerialConsole;

use anyhow::{anyhow, Context, Result};
use core::result::Result as StdResult;
use propolis_client::handmade::{
    api::{
        InstanceEnsureRequestV2, InstanceGetResponse,
        InstanceMigrateInitiateRequest, InstanceProperties, InstanceState,
        InstanceStateRequested, MigrationState,
    },
    Client, Error as PropolisClientError,
};
use slog::Drain;
use thiserror::Error;
use tokio::{sync::mpsc, time::timeout};
use tracing::{info, info_span, instrument, warn, Instrument};
use uuid::Uuid;

use self::vm_config::VmConfig;

pub mod factory;
pub mod server;
pub mod vm_config;

#[derive(Debug, Error)]
pub enum VmStateError {
    #[error("Operation can only be performed on a VM that has been ensured")]
    InstanceNotEnsured,

    #[error(
        "Operation can only be performed on a new VM that has not been ensured"
    )]
    InstanceAlreadyEnsured,
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
    rt: tokio::runtime::Runtime,
    client: Client,
    server: server::PropolisServer,
    config: VmConfig,
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
    pub(crate) fn new<T: Into<Stdio> + Debug>(
        vm_name: &str,
        process_params: server::ServerProcessParameters<T>,
        vm_config: vm_config::VmConfig,
    ) -> Result<Self> {
        let id = Uuid::new_v4();
        let guest_os_kind = vm_config.guest_os_kind();
        info!(?process_params, ?vm_config, ?guest_os_kind);
        let span = info_span!(parent: None, "VM", vm = ?vm_name, %id);
        let rt =
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

        let server_addr = process_params.server_addr;
        let server = server::PropolisServer::new(process_params)?;

        let client_decorator = slog_term::TermDecorator::new().stdout().build();
        let client_drain =
            slog_term::CompactFormat::new(client_decorator).build().fuse();
        let client_async_drain =
            slog_async::Async::new(client_drain).build().fuse();
        let client = Client::new(
            server_addr.into(),
            slog::Logger::root(client_async_drain, slog::o!()),
        );

        Ok(Self {
            id,
            rt,
            client,
            server,
            config: vm_config,
            guest_os: guest_os::get_guest_os_adapter(guest_os_kind),
            tracing_span: span,
            state: VmState::New,
        })
    }

    /// Obtains a clone of the configuration parameters that were supplied when
    /// this VM was created so that a new VM can be created from them.
    ///
    /// N.B. This also clones handles to the backend objects this VM is using.
    pub(crate) fn clone_config(&self) -> vm_config::VmConfig {
        self.config.clone()
    }

    /// Sends an instance ensure request to this VM's server, allowing it to
    /// transition into the running state.
    async fn instance_ensure_async(
        &self,
        migrate: Option<InstanceMigrateInitiateRequest>,
    ) -> Result<SerialConsole> {
        let _span = self.tracing_span.enter();
        let (vcpus, memory_mib) = match self.state {
            VmState::New => (
                self.config.instance_spec().devices.board.cpus,
                self.config.instance_spec().devices.board.memory_mb,
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
        let ensure_req = InstanceEnsureRequestV2 {
            properties,
            instance_spec: self.config.instance_spec().clone(),
            migrate,
        };

        let mut retries = 3;
        while let Err(e) = self.client.instance_ensure_v2(&ensure_req).await {
            info!("Error {} while creating instance, will retry", e);
            tokio::time::sleep(Duration::from_millis(500)).await;
            retries -= 1;
            if retries == 0 {
                tracing::error!("Failed to create instance after 3 retries");
                anyhow::bail!(e);
            }
        }

        let serial_uri = self.client.instance_serial_console_ws_uri();
        let console = SerialConsole::new(serial_uri).await?;

        let instance_description =
            self.client.instance_get().await.with_context(|| {
                anyhow!("failed to get instance properties")
            })?;

        let instance_spec = self.config.instance_spec();
        info!(
            ?instance_description.instance,
            ?instance_spec,
            "Started instance"
        );

        Ok(console)
    }

    /// Returns the kind of guest OS running in this VM.
    pub fn guest_os_kind(&self) -> GuestOsKind {
        self.config.guest_os_kind()
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
                    self.instance_ensure_async(None).await
                })?;
                self.state = VmState::Ensured { serial: console };
            }
            VmState::Ensured { .. } => {}
        }

        Ok(())
    }

    /// Sets the VM to the running state without first sending an instance
    /// ensure request.
    pub fn run(&self) -> StdResult<(), PropolisClientError> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Run).await
        })
    }

    /// Stops the VM.
    pub fn stop(&self) -> StdResult<(), PropolisClientError> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Stop).await
        })
    }

    /// Resets the VM by requesting the `Reboot` state from the server (as
    /// distinct from requesting a reboot from within the guest).
    pub fn reset(&self) -> StdResult<(), PropolisClientError> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::Reboot).await
        })
    }

    /// Moves this VM into the migrate-start state.
    fn start_migrate(&self) -> StdResult<(), PropolisClientError> {
        self.rt.block_on(async {
            self.put_instance_state_async(InstanceStateRequested::MigrateStart)
                .await
        })
    }

    async fn put_instance_state_async(
        &self,
        state: InstanceStateRequested,
    ) -> StdResult<(), PropolisClientError> {
        let _span = self.tracing_span.enter();
        info!(?state, "Requesting instance state change");
        self.client.instance_state_put(state).await
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
            .await
            .with_context(|| anyhow!("failed to query instance properties"))
    }

    /// Starts this instance by issuing an ensure request that specifies a
    /// migration from `source` and then running the target.
    pub fn migrate_from(
        &mut self,
        source: &Self,
        timeout_duration: Duration,
    ) -> Result<()> {
        let _vm_guard = self.tracing_span.enter();
        match self.state {
            VmState::New => {
                let migration_id = Uuid::new_v4();
                info!(
                    ?migration_id,
                    "Migrating from source at address {}",
                    source.server.server_addr()
                );

                source.start_migrate()?;
                let console = self.rt.block_on(async {
                    self.instance_ensure_async(Some(
                        InstanceMigrateInitiateRequest {
                            migration_id,
                            src_addr: source.server.server_addr().into(),
                            src_uuid: Uuid::default(),
                        },
                    ))
                    .await
                })?;
                self.state = VmState::Ensured { serial: console };

                let span = info_span!("Migrate", ?migration_id);
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
                    .map_err(|e| anyhow!("error during migration: {}", e))?;
            }
            VmState::Ensured { .. } => {
                return Err(VmStateError::InstanceAlreadyEnsured.into());
            }
        }

        self.run()?;
        Ok(())
    }

    fn get_migration_state(
        &self,
        migration_id: Uuid,
    ) -> Result<MigrationState> {
        self.rt.block_on(async {
            Ok(self.client.instance_migrate_status(migration_id).await?.state)
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
                Err(backoff::Error::Transient {
                    err: anyhow!(
                        "not in desired state yet: current {:?}, target {:?}",
                        current,
                        target
                    ),
                    retry_after: None,
                })
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
                        match step {
                            CommandSequenceEntry::WaitFor(s) => {
                                self.wait_for_serial_output_async(
                                    s,
                                    Duration::MAX,
                                )
                                .await?;
                            }
                            CommandSequenceEntry::WriteStr(s) => {
                                self.send_serial_str_async(s).await?
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
        let (preceding_tx, mut preceding_rx) = mpsc::channel(1);
        match &self.state {
            VmState::Ensured { serial } => {
                serial
                    .register_wait_for_string(line.clone(), preceding_tx)
                    .await?;
                let t = timeout(timeout_duration, preceding_rx.recv()).await;
                match t {
                    Err(timeout_elapsed) => {
                        serial.cancel_wait_for_string().await;
                        Err(anyhow!(timeout_elapsed))
                    }
                    Ok(received_string) => Ok(received_string),
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
        self.send_serial_str(cmd)?;

        let mut echo_cmd = cmd.to_string();
        echo_cmd.push('\n');

        self.wait_for_serial_output(&echo_cmd, Duration::from_secs(15))?;
        let mut out = self.wait_for_serial_output(
            self.guest_os.get_shell_prompt(),
            Duration::from_secs(300),
        )?;
        out.truncate(out.trim_end().len());
        Ok(out)
    }

    fn send_serial_str(&self, string: &str) -> Result<()> {
        self.rt.block_on(async { self.send_serial_str_async(string).await })
    }

    async fn send_serial_str_async(&self, string: &str) -> Result<()> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(string.as_bytes());
        self.send_serial_bytes_async(bytes).await?;
        self.send_serial_bytes_async(vec![b'\n']).await
    }

    async fn send_serial_bytes_async(&self, bytes: Vec<u8>) -> Result<()> {
        match &self.state {
            VmState::Ensured { serial } => serial.send_bytes(bytes).await,
            VmState::New => Err(VmStateError::InstanceNotEnsured.into()),
        }
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
