// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use anyhow::{anyhow, Context};
use clap::Parser;
use dropshot::{ConfigDropshot, HandlerTaskMode, HttpServerStarter};
use futures::join;
use propolis::usdt::register_probes;
use slog::{debug, info, Logger};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use propolis_server::{
    config,
    server::{self, MetricsEndpointConfig},
    vnc::setup_vnc,
};

/// Threads to spawn for tokio runtime handling the API (dropshot, etc)
const API_RT_THREADS: usize = 4;

/// Configuration for metric registration.
#[derive(Clone, Debug)]
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
        match s {
            "disable" => Ok(Self::Disable),
            "dns" => Ok(Self::Dns),
            other => {
                let Ok(addr) = other.parse() else {
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
        cfg: PathBuf,

        #[clap(name = "PROPOLIS_IP:PORT", action)]
        propolis_addr: SocketAddr,

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
        metrics_addr: MetricRegistration,

        #[clap(
            name = "VNC_IP:PORT",
            default_value_t = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 5900),
            action
        )]
        vnc_addr: SocketAddr,

        /// Logging level for the server
        #[clap(long, default_value_t = slog::Level::Info, value_parser = parse_log_level)]
        log_level: slog::Level,
    },
}

pub fn run_openapi() -> Result<(), String> {
    server::api()
        .openapi("Oxide Propolis Server API", "0.0.1")
        .description(
            "API for interacting with the Propolis hypervisor frontend.",
        )
        .contact_url("https://oxide.computer")
        .contact_email("api@oxide.computer")
        .write(&mut std::io::stdout())
        .map_err(|e| e.to_string())
}

fn run_server(
    config_app: config::Config,
    config_dropshot: dropshot::ConfigDropshot,
    config_metrics: Option<MetricsEndpointConfig>,
    vnc_addr: SocketAddr,
    log: slog::Logger,
) -> anyhow::Result<()> {
    use propolis::api_version;

    // Check that devices conform to expected API version
    if let Err(e) = api_version::check() {
        if let api_version::Error::Io(ioe) = &e {
            if ioe.kind() == std::io::ErrorKind::NotFound {
                slog::error!(log, "Failed to open /dev/vmmctl");
            }
        }

        Err(e).context("API version checks")?;
    }

    let vnc_server = setup_vnc(&log, vnc_addr);
    let vnc_server_hdl = vnc_server.clone();
    let use_reservoir = config::reservoir_decide(&log);

    let context = server::DropshotEndpointContext::new(
        config_app,
        vnc_server,
        use_reservoir,
        log.new(slog::o!()),
        config_metrics,
    );

    // Spawn the runtime for handling API processing
    // If/when a VM instance is created, a separate runtime for handling device
    // emulation and other VM-related work will be spawned.
    let api_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(API_RT_THREADS)
        .enable_all()
        .thread_name("tokio-rt-api")
        .build()?;
    let _guard = api_runtime.enter();

    info!(log, "Starting server...");

    let server = HttpServerStarter::new(
        &config_dropshot,
        server::api(),
        Arc::new(context),
        &log,
    )
    .map_err(|error| anyhow!("Failed to start server: {}", error))?
    .start();

    let server_res =
        api_runtime.block_on(async { join!(server, vnc_server_hdl.start()).0 });

    server_res.map_err(|e| anyhow!("Server exited with an error: {}", e))
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
    metrics_addr: MetricRegistration,
    listen_addr: IpAddr,
) -> anyhow::Result<Option<MetricsEndpointConfig>> {
    let cfg = match metrics_addr {
        MetricRegistration::Disable => {
            debug!(
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
            Some(MetricsEndpointConfig::new(listen_addr, None))
        }
        MetricRegistration::WithAddr(addr) => {
            Some(MetricsEndpointConfig::new(listen_addr, Some(addr)))
        }
    };
    Ok(cfg)
}

fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    // Command line arguments.
    let args = Args::parse();

    match args {
        Args::OpenApi => run_openapi()
            .map_err(|e| anyhow!("Cannot generate OpenAPI spec: {}", e)),
        Args::Run { cfg, propolis_addr, metrics_addr, vnc_addr, log_level } => {
            let config = config::parse(cfg)?;

            // Dropshot configuration.
            let config_dropshot = ConfigDropshot {
                bind_address: propolis_addr,
                request_body_max_bytes: 1024 * 1024, // 1M for ISO bytes
                default_handler_task_mode: HandlerTaskMode::Detached,
                log_headers: vec![],
            };

            let log = build_logger(log_level);

            let metric_config = build_metric_configuration(
                &log,
                metrics_addr,
                propolis_addr.ip(),
            )?;

            run_server(config, config_dropshot, metric_config, vnc_addr, log)
        }
    }
}
