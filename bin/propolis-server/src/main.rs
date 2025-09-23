// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use propolis::usdt::register_probes;
use propolis_server::{
    config,
    server::{self, MetricsEndpointConfig},
    vnc,
};

use anyhow::{anyhow, Context};
use clap::Parser;
use dropshot::{ConfigDropshot, HandlerTaskMode, HttpServerStarter};
use slog::{info, Logger};

/// Threads to spawn for tokio runtime handling the API (dropshot, etc)
const API_RT_THREADS: usize = 4;

/// Configuration for metric registration.
#[derive(Clone, Debug, PartialEq)]
enum MetricRegistration {
    Disable,
    Dns,
    WithAddr(SocketAddr),
}

impl fmt::Display for MetricRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricRegistration::Disable => "disable".fmt(f),
            MetricRegistration::Dns => "dns".fmt(f),
            MetricRegistration::WithAddr(addr) => addr.fmt(f),
        }
    }
}

impl FromStr for MetricRegistration {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("disable") {
            Ok(Self::Disable)
        } else if s.eq_ignore_ascii_case("dns") {
            Ok(Self::Dns)
        } else {
            let Ok(addr) = s.parse() else {
                anyhow::bail!(
                    "Metric registration must be 'disable', \
                    'dns', or an explicit socket address \
                    written as `IP:port`",
                );
            };
            Ok(Self::WithAddr(addr))
        }
    }
}

fn parse_log_level(s: &str) -> anyhow::Result<slog::Level> {
    s.parse().map_err(|_| anyhow::anyhow!("Invalid log level"))
}

#[derive(Debug, Parser)]
#[clap(about, version)]
/// An HTTP server providing access to Propolis
enum Args {
    /// Generates the OpenAPI specification.
    OpenApi,
    /// Runs the Propolis server.
    Run {
        #[clap(action)]
        bootrom_path: PathBuf,

        #[clap(name = "PROPOLIS_IP:PORT", action)]
        propolis_addr: SocketAddr,

        #[clap(long, action)]
        bootrom_version: Option<String>,

        /// Method for registering as an Oximeter metric producer.
        ///
        /// The following values are supported:
        ///
        /// disable - Do not register or attempt to produce metrics.
        ///
        /// dns - Register at an address inferred from Oxide internal DNS.
        /// This is only available if the Propolis is listening on a
        /// non-localhost IPv6 address.
        ///
        /// IP:port - Register with the explicitly-provided socket address.
        #[clap(long, default_value_t = MetricRegistration::Disable)]
        metric_addr: MetricRegistration,

        /// IP:Port for raw TCP access to VNC console
        #[clap(name = "VNC_IP:PORT", action)]
        vnc_addr: Option<SocketAddr>,

        /// Logging level for the server
        #[clap(long, default_value_t = slog::Level::Info, value_parser = parse_log_level)]
        log_level: slog::Level,
    },
}

pub fn run_openapi() -> Result<(), String> {
    server::api()
        .openapi("Oxide Propolis Server API", semver::Version::new(0, 0, 1))
        .description(
            "API for interacting with the Propolis hypervisor frontend.",
        )
        .contact_url("https://oxide.computer")
        .contact_email("api@oxide.computer")
        .write(&mut std::io::stdout())
        .map_err(|e| e.to_string())
}

fn run_server(
    bootrom_path: PathBuf,
    bootrom_version: Option<String>,
    config_dropshot: dropshot::ConfigDropshot,
    config_metrics: Option<MetricsEndpointConfig>,
    vnc_addr: Option<SocketAddr>,
    log: slog::Logger,
) -> anyhow::Result<()> {
    use propolis::api_version;

    // Check that devices conform to expected API version
    if let Err(e) = api_version::check() {
        use api_version::{Error, VersionCheckError};
        if let VersionCheckError { component: _, path, err: Error::Io(ioe) } =
            &e
        {
            if ioe.kind() == std::io::ErrorKind::NotFound {
                slog::error!(log, "Failed to open {path}");
            }
        }

        Err(e).context("API version checks")?;
    }

    // If this is a development image being run outside of an Omicron zone,
    // enable the display (in logs, panic messages, and the like) of diagnostic
    // data that may have originated in the guest.
    #[cfg(not(feature = "omicron-build"))]
    propolis::common::DISPLAY_GUEST_DATA
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let use_reservoir = config::reservoir_decide(&log);

    let context = server::DropshotEndpointContext::new(
        bootrom_path,
        bootrom_version,
        use_reservoir,
        log.new(slog::o!()),
        config_metrics,
    );

    // Spawn the runtime for handling API processing
    // If/when a VM instance is created, a separate runtime for handling device
    // emulation and other VM-related work will be spawned.
    let api_runtime = {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(API_RT_THREADS).thread_name("tokio-rt-api");
        oxide_tokio_rt::build(&mut builder)?
    };
    let _guard = api_runtime.enter();

    // Start TCP listener for VNC, if requested
    let tcp_vnc = match vnc_addr {
        Some(addr) => Some(api_runtime.block_on(async {
            vnc::TcpSock::new(context.vnc_server.clone(), addr, log.clone())
                .await
        })?),
        None => None,
    };

    info!(log, "Starting server...");

    let server = HttpServerStarter::new(
        &config_dropshot,
        server::api(),
        Arc::new(context),
        &log,
    )
    .map_err(|error| anyhow!("Failed to start server: {}", error))?
    .start();

    let result = api_runtime.block_on(server);

    // Clean up any VNC TCP socket
    if let Some(vnc) = tcp_vnc {
        api_runtime.block_on(async { vnc.halt().await });
    }

    result.map_err(|e| anyhow!("Server exited with an error: {}", e))
}

fn build_logger(level: slog::Level) -> slog::Logger {
    use slog::Drain;

    let main_drain = if atty::is(atty::Stream::Stdout) {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    } else {
        let drain =
            slog_bunyan::with_name("propolis-server", std::io::stdout())
                .build()
                .fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    };

    let (dtrace_drain, probe_reg) = slog_dtrace::Dtrace::new();

    let filtered_main = slog::LevelFilter::new(main_drain, level);

    let log = slog::Logger::root(
        slog::Duplicate::new(filtered_main.fuse(), dtrace_drain.fuse()).fuse(),
        slog::o!(),
    );

    if let slog_dtrace::ProbeRegistration::Failed(err) = probe_reg {
        slog::error!(&log, "Error registering slog-dtrace probes: {:?}", err);
    }

    log
}

fn is_valid_listen_addr_for_dns(listen_addr: IpAddr) -> bool {
    let IpAddr::V6(addr) = listen_addr else {
        return false;
    };
    addr != Ipv6Addr::LOCALHOST
}

/// Build metric configuration from the provided registration and listen
/// addresses.
///
/// This will return None if metrics are explicitly disabled.
fn build_metric_configuration(
    log: &Logger,
    metric_addr: MetricRegistration,
    listen_addr: IpAddr,
) -> anyhow::Result<Option<MetricsEndpointConfig>> {
    let cfg = match metric_addr {
        MetricRegistration::Disable => {
            info!(
                log,
                "metric registration is disabled, no metric \
                data will be produced by this server",
            );
            None
        }
        MetricRegistration::Dns => {
            anyhow::ensure!(
                is_valid_listen_addr_for_dns(listen_addr),
                "Metric registration can only use DNS \
                if the Propolis server is provided a \
                non-localhost IPv6 address"
            );
            Some(MetricsEndpointConfig { listen_addr, registration_addr: None })
        }
        MetricRegistration::WithAddr(addr) => Some(MetricsEndpointConfig {
            listen_addr,
            registration_addr: Some(addr),
        }),
    };
    Ok(cfg)
}

fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    #[cfg(all(
        feature = "omicron-build",
        any(feature = "failure-injection", feature = "falcon")
    ))]
    if option_env!("PHD_BUILD") != Some("true") {
        panic!(
            "`omicron-build` is enabled alongside development features, \
            this build is NOT SUITABLE for production. Set PHD_BUILD=true in \
            the environment and rebuild propolis-server if you really need \
            this to work."
        );
    }

    // Command line arguments.
    let args = Args::parse();

    match args {
        Args::OpenApi => run_openapi()
            .map_err(|e| anyhow!("Cannot generate OpenAPI spec: {}", e)),
        Args::Run {
            bootrom_path,
            bootrom_version,
            propolis_addr,
            metric_addr,
            vnc_addr,
            log_level,
        } => {
            // Dropshot configuration.
            let config_dropshot = ConfigDropshot {
                bind_address: propolis_addr,
                default_request_body_max_bytes: 1024 * 1024, // 1M for ISO bytes
                default_handler_task_mode: HandlerTaskMode::Detached,
                log_headers: vec![],
            };

            let log = build_logger(log_level);

            let metric_config = build_metric_configuration(
                &log,
                metric_addr,
                propolis_addr.ip(),
            )?;

            run_server(
                bootrom_path,
                bootrom_version,
                config_dropshot,
                metric_config,
                vnc_addr,
                log,
            )
        }
    }
}
