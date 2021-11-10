use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::prelude::AsRawFd,
};

use anyhow::{anyhow, bail, Context};
use futures::{SinkExt, StreamExt};
use propolis_client::{
    api::{
        DiskRequest, InstanceEnsureRequest, InstanceProperties,
        InstanceStateRequested, Slot,
    },
    Client,
};
use slog::{o, Drain, Level, Logger};
use structopt::StructOpt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "propolis-cli",
    about = "A simple CLI tool to manipulate propolis-server"
)]
struct Opt {
    /// propolis-server address
    #[structopt(short, long, parse(try_from_str = resolve_host))]
    server: IpAddr,

    /// propolis-server port
    #[structopt(short, long, default_value = "12400")]
    port: u16,

    /// Enable debugging
    #[structopt(short, long)]
    debug: bool,

    #[structopt(subcommand)]
    cmd: Command,
}

#[derive(Debug, StructOpt)]
enum Command {
    /// Create a new propolis instance
    New {
        /// Instance name
        name: String,

        /// Number of vCPUs allocated to instance
        #[structopt(short = "c", default_value = "4")]
        vcpus: u8,

        /// Memory allocated to instance (MiB)
        #[structopt(short, default_value = "1024")]
        memory: u64,

        // Specify multiple disks in groups of 3 targets
        #[structopt(long, use_delimiter = true)]
        crucible_target: Vec<SocketAddr>,
    },

    /// Get the properties of a propolis instance
    Get {
        /// Instance name
        name: String,
    },

    /// Transition the instance to a new state
    State {
        /// Instance name
        name: String,

        /// The requested state
        #[structopt(parse(try_from_str = parse_state))]
        state: InstanceStateRequested,
    },

    /// Drop to a Serial console connected to the instance
    Serial {
        /// Instance name
        name: String,
    },
}

fn parse_state(state: &str) -> anyhow::Result<InstanceStateRequested> {
    match state.to_lowercase().as_str() {
        "run" => Ok(InstanceStateRequested::Run),
        "stop" => Ok(InstanceStateRequested::Stop),
        "reboot" => Ok(InstanceStateRequested::Reboot),
        _ => Err(anyhow!(
            "invalid requested state, must be one of: 'run', 'stop', 'reboot"
        )),
    }
}

/// Given a string representing an host, attempts to resolve it to a specific IP address
fn resolve_host(server: &str) -> anyhow::Result<IpAddr> {
    (server, 0)
        .to_socket_addrs()?
        .map(|sock_addr| sock_addr.ip())
        .next()
        .ok_or_else(|| {
            anyhow!("failed to resolve server argument '{}'", server)
        })
}

/// Create a top-level logger that outputs to stderr
fn create_logger(opt: &Opt) -> Logger {
    let decorator = slog_term::TermDecorator::new().stderr().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let level = if opt.debug { Level::Debug } else { Level::Info };
    let drain = slog::LevelFilter(drain, level).fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = Logger::root(drain, o!());
    logger
}

async fn new_instance(
    client: &Client,
    name: String,
    vcpus: u8,
    memory: u64,
    crucible_target: Vec<SocketAddr>,
) -> anyhow::Result<()> {
    if !crucible_target.is_empty() {
        if crucible_target.len() % 3 != 0 {
            bail!("Multiple of three crucible targets required!");
        }
    }

    // Generate a UUID for the new instance
    let id = Uuid::new_v4();

    let properties = InstanceProperties {
        id,
        name,
        description: "propolis-cli generated instance".to_string(),
        // TODO: Use real UUID
        image_id: Uuid::default(),
        // TODO: Use real UUID
        bootrom_id: Uuid::default(),
        memory,
        vcpus,
    };

    let mut disks: Vec<DiskRequest> =
        Vec::with_capacity(crucible_target.len() / 3);

    let mut disk_i = 0;
    for target_group in crucible_target.chunks(3) {
        assert_eq!(target_group.len(), 3);
        disks.push(DiskRequest {
            name: format!("d{}", disk_i),
            address: target_group.to_vec(),
            slot: Slot(disk_i),
            read_only: false,
            key: None,
        });
        disk_i += 1;
    }

    let request = InstanceEnsureRequest {
        properties,
        // TODO: Allow specifying NICs
        nics: vec![],
        disks,
    };

    // Try to create the instance
    client
        .instance_ensure(&request)
        .await
        .with_context(|| anyhow!("failed to create instance"))?;

    Ok(())
}

async fn get_instance(client: &Client, name: String) -> anyhow::Result<()> {
    // Grab the Instance UUID
    let id = client
        .instance_get_uuid(&name)
        .await
        .with_context(|| anyhow!("failed to get instance UUID"))?;

    // Get the rest of the Instance properties
    let res = client
        .instance_get(id)
        .await
        .with_context(|| anyhow!("failed to get instance properties"))?;

    println!("{:#?}", res.instance);

    Ok(())
}

async fn put_instance(
    client: &Client,
    name: String,
    state: InstanceStateRequested,
) -> anyhow::Result<()> {
    // Grab the Instance UUID
    let id = client
        .instance_get_uuid(&name)
        .await
        .with_context(|| anyhow!("failed to get instance UUID"))?;

    client
        .instance_state_put(id, state)
        .await
        .with_context(|| anyhow!("failed to set instance state"))?;

    Ok(())
}

async fn serial(
    client: &Client,
    addr: SocketAddr,
    name: String,
) -> anyhow::Result<()> {
    // Grab the Instance UUID
    let id = client
        .instance_get_uuid(&name)
        .await
        .with_context(|| anyhow!("failed to get instance UUID"))?;

    let path = format!("ws://{}/instances/{}/serial", addr, id);
    let (mut ws, _) = tokio_tungstenite::connect_async(path)
        .await
        .with_context(|| anyhow!("failed to create serial websocket stream"))?;

    let _raw_guard = RawTermiosGuard::stdio_guard()
        .with_context(|| anyhow!("failed to set raw mode"))?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            c = stdin.read_u8() => {
                match c? {
                    // Exit on Ctrl-C
                    b'\x03' => break,
                    c => ws.send(Message::binary(vec![c])).await?,
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(input))) => {
                        stdout.write_all(&input).await?;
                        stdout.flush().await?;
                    }
                    Some(Ok(Message::Close(..))) | None => break,
                    _ => continue,
                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();
    let log = create_logger(&opt);

    let addr = SocketAddr::new(opt.server, opt.port);
    let client = Client::new(addr.clone(), log.new(o!()));

    match opt.cmd {
        Command::New { name, vcpus, memory, crucible_target } => {
            new_instance(
                &client,
                name.to_string(),
                vcpus,
                memory,
                crucible_target,
            )
            .await?
        }
        Command::Get { name } => get_instance(&client, name).await?,
        Command::State { name, state } => {
            put_instance(&client, name, state).await?
        }
        Command::Serial { name } => serial(&client, addr, name).await?,
    }

    Ok(())
}

/// Guard object that will set the terminal to raw mode and restore it
/// to its previous state when it's dropped
struct RawTermiosGuard(libc::c_int, libc::termios);

impl RawTermiosGuard {
    fn stdio_guard() -> Result<RawTermiosGuard, std::io::Error> {
        let fd = std::io::stdout().as_raw_fd();
        let termios = unsafe {
            let mut curr_termios = std::mem::zeroed();
            let r = libc::tcgetattr(fd, &mut curr_termios);
            if r == -1 {
                return Err(std::io::Error::last_os_error());
            }
            curr_termios
        };
        let guard = RawTermiosGuard(fd, termios.clone());
        unsafe {
            let mut raw_termios = termios;
            libc::cfmakeraw(&mut raw_termios);
            let r = libc::tcsetattr(fd, libc::TCSAFLUSH, &raw_termios);
            if r == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(guard)
    }
}
impl Drop for RawTermiosGuard {
    fn drop(&mut self) {
        let r = unsafe { libc::tcsetattr(self.0, libc::TCSADRAIN, &self.1) };
        if r == -1 {
            Err::<(), _>(std::io::Error::last_os_error()).unwrap();
        }
    }
}
