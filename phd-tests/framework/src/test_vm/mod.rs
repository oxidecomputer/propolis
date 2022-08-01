//! Routines for starting VMs, changing their states, and interacting with their
//! guest OSes.

use std::{fmt::Debug, process::Stdio, time::Duration};

use crate::guest_os::{self, CommandSequenceEntry, GuestOs, GuestOsKind};
use crate::serial::SerialConsole;

use anyhow::{anyhow, Context, Result};
use backoff::backoff::Backoff;
use core::result::Result as StdResult;
use propolis_client::api::InstanceState;
use propolis_client::{
    api::{
        InstanceEnsureRequest, InstanceGetResponse, InstanceProperties,
        InstanceStateRequested,
    },
    Client, Error as PropolisClientError,
};
use slog::Drain;
use tokio::{sync::mpsc, time::timeout};
use tracing::{info, info_span, instrument, Instrument};
use uuid::Uuid;

pub mod factory;
pub mod server;
pub mod vm_config;

/// A virtual machine running in a Propolis server. Test cases create these VMs
/// using the [`factory::VmFactory`] embedded in their test contexts.
///
/// Once a VM has been created, tests will usually want to issue [`TestVm::run`]
/// and [`TestVm::wait_to_boot`] calls so they can begin interacting with the
/// serial console.
pub struct TestVm {
    rt: tokio::runtime::Runtime,
    client: Client,
    _server: server::PropolisServer,
    serial: SerialConsole,
    guest_os: Box<dyn GuestOs>,
    guest_os_kind: GuestOsKind,
    tracing_span: tracing::Span,
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
        vm_config: &vm_config::VmConfig,
        guest_os_kind: GuestOsKind,
    ) -> Result<Self> {
        info!(?process_params, ?vm_config, ?guest_os_kind);
        let vm_id = Uuid::new_v4();
        let span = info_span!(parent: None, "VM", vm = ?vm_name);
        let rt =
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

        let server_addr = process_params.server_addr.clone();
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

        let console: SerialConsole = rt.block_on(
            async {
                Self::instance_ensure(
                    &client,
                    vm_id,
                    vm_config.cpus(),
                    vm_config.memory_mib(),
                )
                .await
            }
            .instrument(span.clone()),
        )?;

        Ok(Self {
            rt,
            client,
            _server: server,
            serial: console,
            guest_os: guest_os::get_guest_os_adapter(guest_os_kind),
            guest_os_kind,
            tracing_span: span,
        })
    }

    async fn instance_ensure(
        client: &Client,
        vm_id: Uuid,
        vcpus: u8,
        memory_mib: u64,
    ) -> Result<SerialConsole> {
        let properties = InstanceProperties {
            id: vm_id,
            name: format!("phd-vm-{}", vm_id),
            description: "Pheidippides-managed VM".to_string(),
            image_id: Uuid::default(),
            bootrom_id: Uuid::default(),
            memory: memory_mib,
            vcpus,
        };
        let ensure_req = InstanceEnsureRequest {
            properties,
            nics: vec![],
            disks: vec![],
            migrate: None,
            cloud_init_bytes: None,
        };

        if let Err(e) = client.instance_ensure(&ensure_req).await {
            info!("Error {} while creating instance, will retry", e);
            tokio::time::sleep(Duration::from_millis(500)).await;
            client.instance_ensure(&ensure_req).await?;
        }

        let serial_uri = client.instance_serial_console_ws_uri();
        let console = SerialConsole::new(serial_uri).await?;

        let instance_description = client
            .instance_get()
            .await
            .with_context(|| anyhow!("failed to get instance properties"))?;

        info!(
            ?instance_description.instance,
            "Started instance"
        );

        anyhow::Ok(console)
    }

    /// Returns the kind of guest OS running in this VM.
    pub fn guest_os_kind(&self) -> GuestOsKind {
        self.guest_os_kind
    }

    /// Starts the VM.
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

        let mut backoff = backoff::ExponentialBackoff::default();
        backoff.max_elapsed_time = Some(timeout_duration);
        loop {
            let current = self.get()?.instance.state;
            if current == target {
                return Ok(());
            }

            match backoff.next_backoff() {
                Some(to_wait) => {
                    info!(
                        "Waiting for state {:?}, got state {:?}, \
                          waiting {:#?} before trying again",
                        target, current, to_wait
                    );
                    std::thread::sleep(to_wait);
                }
                None => {
                    info!("Timed out waiting for state {:?}", target);
                    break;
                }
            }
        }

        Err(anyhow!(
            "Timed out waiting for instance to reach state {:?}",
            target
        ))
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

        received.ok_or(
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Channel unexpectedly closed while waiting for string",
            )
            .into(),
        )
    }

    async fn wait_for_serial_output_async(
        &self,
        line: &str,
        timeout_duration: Duration,
    ) -> Result<Option<String>> {
        let line = line.to_string();
        let (preceding_tx, mut preceding_rx) = mpsc::channel(1);
        self.serial
            .register_wait_for_string(line.clone(), preceding_tx)
            .await?;
        let t = timeout(timeout_duration, preceding_rx.recv()).await;
        match t {
            Err(timeout_elapsed) => {
                self.serial.cancel_wait_for_string().await;
                Err(anyhow!(timeout_elapsed))
            }
            Ok(received_string) => Ok(received_string),
        }
    }

    /// Runs the shell command `cmd` by sending it to the serial console, then
    /// waits for another shell prompt to appear using
    /// [`Self::wait_for_serial_output`] and returns any text that was buffered
    /// to the serial console after the command was sent.
    pub fn run_shell_command(&self, cmd: &str) -> Result<String> {
        self.send_serial_str(cmd)?;

        let mut echo_cmd = cmd.to_string();
        echo_cmd.push_str("\n");

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
        self.serial.send_bytes(bytes).await
    }
}
