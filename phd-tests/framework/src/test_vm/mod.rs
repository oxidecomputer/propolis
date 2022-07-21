//! Routines for starting VMs, changing their states, and interacting with their
//! guest OSes.

use std::{
    fmt::Debug,
    net::{Ipv4Addr, SocketAddrV4},
    process::Stdio,
    time::Duration,
};

use crate::guest_os::{self, CommandSequenceEntry, GuestOs, GuestOsKind};
use crate::serial::SerialConsole;

use anyhow::{anyhow, Context, Result};
use propolis_client::{
    api::{
        InstanceEnsureRequest, InstanceGetResponse, InstanceProperties,
        InstanceStateRequested,
    },
    Client,
};
use slog::Drain;
use tokio::{sync::mpsc, time::timeout};
use tracing::{info, info_span, instrument, Instrument};
use uuid::Uuid;

pub mod factory;
pub mod vm_config;

struct ServerWrapper {
    server: std::process::Child,
}

impl Drop for ServerWrapper {
    fn drop(&mut self) {
        std::process::Command::new("pfexec")
            .args(["kill", self.server.id().to_string().as_str()])
            .spawn()
            .unwrap();
    }
}

pub struct TestVm {
    rt: tokio::runtime::Runtime,
    client: Client,
    _server: ServerWrapper,
    serial: SerialConsole,
    guest_os: Box<dyn GuestOs>,
    guest_os_kind: GuestOsKind,
    tracing_span: tracing::Span,
}

#[derive(Debug)]
pub struct ServerProcessParameters<'a, T: Into<Stdio> + Debug> {
    pub server_path: &'a str,
    pub config_toml_path: &'a str,
    pub server_addr: SocketAddrV4,
    pub server_stdout: T,
    pub server_stderr: T,
}

impl TestVm {
    #[instrument(skip_all)]
    pub(crate) fn new<T: Into<Stdio> + Debug>(
        vm_name: &str,
        process_params: ServerProcessParameters<T>,
        vm_config: &vm_config::VmConfig,
        guest_os_kind: GuestOsKind,
    ) -> Result<Self> {
        info!(?process_params, ?vm_config, ?guest_os_kind);
        let vm_id = Uuid::new_v4();
        let span = info_span!("VM", name = ?vm_name);
        let rt =
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

        let ServerProcessParameters {
            server_path,
            config_toml_path,
            server_addr,
            server_stdout,
            server_stderr,
        } = process_params;

        info!(
            ?server_path,
            ?config_toml_path,
            ?server_addr,
            ?vm_config,
            "Launching Propolis server"
        );
        let server = ServerWrapper {
            server: std::process::Command::new("pfexec")
                .args([
                    server_path,
                    "run",
                    config_toml_path,
                    server_addr.to_string().as_str(),
                ])
                .stdout(server_stdout)
                .stderr(server_stderr)
                .spawn()?,
        };

        info!("Launched server with pid {}", server.server.id());

        let client_decorator = slog_term::TermDecorator::new().stdout().build();
        let client_drain =
            slog_term::CompactFormat::new(client_decorator).build().fuse();
        let client_async_drain =
            slog_async::Async::new(client_drain).build().fuse();
        let client = Client::new(
            std::net::SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                9000,
            )),
            slog::Logger::root(client_async_drain, slog::o!()),
        );

        let console: SerialConsole = rt.block_on(
            async {
                let properties = InstanceProperties {
                    id: vm_id,
                    name: "phd-vm".to_string(),
                    description: "Pheidippides-managed VM".to_string(),
                    image_id: Uuid::default(),
                    bootrom_id: Uuid::default(),
                    memory: vm_config.memory_mib(),
                    vcpus: vm_config.cpus(),
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

                let instance_description =
                    client.instance_get().await.with_context(|| {
                        anyhow!("failed to get instance properties")
                    })?;

                info!(
                    ?instance_description.instance,
                    "Started instance"
                );

                anyhow::Ok(console)
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

    pub fn guest_os_kind(&self) -> GuestOsKind {
        self.guest_os_kind
    }

    pub fn run(&self) -> Result<()> {
        let _span = self.tracing_span.enter();
        info!("Sending run request to server");
        self.rt.block_on(async {
            self.client
                .instance_state_put(InstanceStateRequested::Run)
                .await
                .with_context(|| {
                anyhow!("failed to set instance state to running")
            })?;

            Ok(())
        })
    }

    pub fn get(&self) -> Result<InstanceGetResponse> {
        let _span = self.tracing_span.enter();
        info!("Sending instance get request to server");
        self.rt.block_on(async {
            let res = self.client.instance_get().await.with_context(|| {
                anyhow!("failed to query instance properties")
            })?;
            Ok(res)
        })
    }

    pub fn wait_to_boot(&self) -> Result<()> {
        let timeout_duration = Duration::from_secs(300);
        let wait_span =
            info_span!("Waiting {} for guest to boot", ?timeout_duration);
        wait_span.follows_from(&self.tracing_span);

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
                .instrument(wait_span),
            )
            .await
            .map_err(|e| anyhow!(e))
        })?;

        info!("Guest has booted");
        Ok(())
    }

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
