// Required for USDT
#![cfg_attr(feature = "dtrace-probes", feature(asm))]
#![cfg_attr(
    all(feature = "dtrace-probes", target_os = "macos"),
    feature(asm_sym)
)]

use anyhow::anyhow;
use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingLevel, HttpServerStarter,
};
use propolis::usdt::register_probes;
use slog::info;
use std::net::SocketAddr;
use std::path::PathBuf;
use structopt::StructOpt;

use propolis_server::{config, server};

#[derive(Debug, StructOpt)]
#[structopt(
    name = "propolis-server",
    about = "An HTTP server providing access to Propolis"
)]
enum Args {
    /// Generates the OpenAPI specification.
    OpenApi,
    /// Runs the Propolis server.
    Run {
        #[structopt(parse(from_os_str))]
        cfg: PathBuf,

        #[structopt(name = "PROPOLIS_IP:PORT", parse(try_from_str))]
        propolis_addr: SocketAddr,
    },
}

pub fn run_openapi() -> Result<(), String> {
    server::api()
        .openapi("Oxide Propolis API", "0.0.1")
        .description("Api for interacting with Propolis agent")
        .contact_url("https://oxide.computer")
        .contact_email("api@oxide.computer")
        .write(&mut std::io::stdout())
        .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    // Command line arguments.
    let args = Args::from_args_safe()?;

    match args {
        Args::OpenApi => run_openapi()
            .map_err(|e| anyhow!("Cannot generate OpenAPI spec: {}", e)),
        Args::Run { cfg, propolis_addr } => {
            let config = config::parse(&cfg)?;

            // Dropshot configuration.
            let config_dropshot = ConfigDropshot {
                bind_address: propolis_addr,
                ..Default::default()
            };
            let config_logging = ConfigLogging::StderrTerminal {
                level: ConfigLoggingLevel::Info,
            };
            let log = config_logging.to_logger("propolis-server").map_err(
                |error| anyhow!("failed to create logger: {}", error),
            )?;

            let context = server::Context::new(config, log.new(slog::o!()));
            info!(log, "Starting server...");
            let server = HttpServerStarter::new(
                &config_dropshot,
                server::api(),
                context,
                &log,
            )
            .map_err(|error| anyhow!("Failed to start server: {}", error))?
            .start();
            server
                .await
                .map_err(|e| anyhow!("Server exited with an error: {}", e))
        }
    }
}
