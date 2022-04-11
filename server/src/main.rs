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
use futures::join;
use propolis::usdt::register_probes;
use rfb::rfb::{ProtoVersion, SecurityType, SecurityTypes};
use rfb::server::VncServer;
use rfb::server::{VncServerConfig, VncServerData};
use slog::{info, o, Logger};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use structopt::StructOpt;

use propolis_server::vnc::PropolisVncServer;
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

fn setup_vnc(log: &Logger) -> VncServer<PropolisVncServer> {
    // XXX: Do we want to specify this information in the config file?
    let initial_width = 1024;
    let initial_height = 768;

    let config = VncServerConfig {
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 5900),
        version: ProtoVersion::Rfb38,
        // vncviewer won't work without offering VncAuth, even though it doesn't ask to use
        // it.
        sec_types: SecurityTypes(vec![
            SecurityType::None,
            SecurityType::VncAuthentication,
        ]),
        name: "propolis-vnc".to_string(),
    };
    let data = VncServerData { width: initial_width, height: initial_height };
    let pvnc = PropolisVncServer::new(
        initial_width,
        initial_height,
        log.new(o!("component" => "vnc-server")),
    );

    VncServer::new(pvnc, config, data)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Ensure proper setup of USDT probes
    register_probes().unwrap();

    // Command line arguments.
    let args = Args::from_args();

    match args {
        Args::OpenApi => run_openapi()
            .map_err(|e| anyhow!("Cannot generate OpenAPI spec: {}", e)),
        Args::Run { cfg, propolis_addr } => {
            let config = config::parse(&cfg)?;

            // Dropshot configuration.
            let config_dropshot = ConfigDropshot {
                bind_address: propolis_addr,
                request_body_max_bytes: 1024 * 1024, // 1M for ISO bytes
                ..Default::default()
            };
            let config_logging = ConfigLogging::StderrTerminal {
                level: ConfigLoggingLevel::Info,
            };
            let log = config_logging.to_logger("propolis-server").map_err(
                |error| anyhow!("failed to create logger: {}", error),
            )?;

            let vnc_server = setup_vnc(&log);
            let vnc_server_hdl = vnc_server.clone();

            let context =
                server::Context::new(config, vnc_server, log.new(slog::o!()));

            info!(log, "Starting server...");
            let server = HttpServerStarter::new(
                &config_dropshot,
                server::api(),
                context,
                &log,
            )
            .map_err(|error| anyhow!("Failed to start server: {}", error))?
            .start();

            let (server_res, _) = join!(server, vnc_server_hdl.start());
            server_res
                .map_err(|e| anyhow!("Server exited with an error: {}", e))
        }
    }
}
