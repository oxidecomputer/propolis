// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::SocketAddr;
use std::path::PathBuf;
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
use slog::info;

/// Threads to spawn for tokio runtime handling the API (dropshot, etc)
const API_RT_THREADS: usize = 4;

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

        /// IP:Port with which to register as an Oximeter metric producer
        #[clap(long, action)]
        metric_addr: Option<SocketAddr>,

        /// IP:Port for raw TCP access to VNC console
        #[clap(name = "VNC_IP:PORT", action)]
        vnc_addr: Option<SocketAddr>,
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
    metrics_addr: Option<SocketAddr>,
    vnc_addr: Option<SocketAddr>,
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

    let use_reservoir = config::reservoir_decide(&log);

    let config_metrics = metrics_addr.map(|addr| {
        let imc =
            MetricsEndpointConfig::new(config_dropshot.bind_address, addr);
        info!(log, "Metrics server will use {:?}", imc);
        imc
    });

    let context = server::DropshotEndpointContext::new(
        config_app,
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

fn build_logger() -> slog::Logger {
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

    let filtered_main = slog::LevelFilter::new(main_drain, slog::Level::Info);

    let log = slog::Logger::root(
        slog::Duplicate::new(filtered_main.fuse(), dtrace_drain.fuse()).fuse(),
        slog::o!(),
    );

    if let slog_dtrace::ProbeRegistration::Failed(err) = probe_reg {
        slog::error!(&log, "Error registering slog-dtrace probes: {:?}", err);
    }

    log
}

fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    // Command line arguments.
    let args = Args::parse();

    match args {
        Args::OpenApi => run_openapi()
            .map_err(|e| anyhow!("Cannot generate OpenAPI spec: {}", e)),
        Args::Run { cfg, propolis_addr, metric_addr, vnc_addr } => {
            let config = config::parse(cfg)?;

            // Dropshot configuration.
            let config_dropshot = ConfigDropshot {
                bind_address: propolis_addr,
                request_body_max_bytes: 1024 * 1024, // 1M for ISO bytes
                default_handler_task_mode: HandlerTaskMode::Detached,
                log_headers: vec![],
            };

            let log = build_logger();

            run_server(config, config_dropshot, metric_addr, vnc_addr, log)
        }
    }
}
