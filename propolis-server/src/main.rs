// Required for USDT
#![feature(asm)]

use anyhow::anyhow;
use dropshot::{
    ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpServerStarter
};
use std::net::SocketAddr;
use std::path::PathBuf;
use structopt::StructOpt;
use propolis::usdt::register_probes;

mod config;
mod initializer;
mod serial;
mod server;
#[cfg(test)]
mod test_utils;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "propolis-server",
    about = "An HTTP server providing access to Propolis"
)]
struct Args {
    #[structopt(parse(from_os_str))]
    cfg: PathBuf,

    #[structopt(name = "PROPOLIS_IP:PORT", parse(try_from_str))]
    propolis_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    // Command line arguments.
    let args = Args::from_args_safe()?;
    let config = config::parse(&args.cfg)?;

    // Dropshot configuration.
    let config_dropshot = ConfigDropshot {
        bind_address: args.propolis_addr,
        ..Default::default()
    };
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("propolis-server")
        .map_err(|error| anyhow!("failed to create logger: {}", error))?;

    let context = server::Context::new(config);
    let server = HttpServerStarter::new(&config_dropshot, server::api(), context, &log)
        .map_err(|error| anyhow!("Failed to start server: {}", error))?
        .start();
    server.await.map_err(|e| anyhow!("Server exited with an error: {}", e))
}
