use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::prelude::AsRawFd,
    time::Duration,
};

use anyhow::{anyhow, Context};
use futures::{future, SinkExt, StreamExt};
use propolis_client::{
    api::{
        DiskRequest, InstanceEnsureRequest, InstanceMigrateInitiateRequest,
        InstanceProperties, InstanceStateRequested, MigrationState,
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

        /// Instance uuid (if specified)
        #[structopt(short = "u")]
        uuid: Option<Uuid>,

        /// Number of vCPUs allocated to instance
        #[structopt(short = "c", default_value = "4")]
        vcpus: u8,

        /// Memory allocated to instance (MiB)
        #[structopt(short, default_value = "1024")]
        memory: u64,

        // file with a JSON array of DiskRequest structs
        #[structopt(long, parse(from_os_str))]
        crucible_disks: Option<PathBuf>,
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

    /// Migrate instance to new propolis-server
    Migrate {
        /// Instance name
        name: String,

        /// Destination propolis-server address
        #[structopt(parse(try_from_str = resolve_host))]
        dst_server: IpAddr,

        /// Destination propolis-server port
        #[structopt(short = "p", default_value = "12400")]
        dst_port: u16,

        /// Uuid for the destination instance
        #[structopt(short = "u")]
        dst_uuid: Option<Uuid>,
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

fn parse_crucible_disks(path: &Path) -> anyhow::Result<Vec<DiskRequest>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).map_err(|e| e.into())
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

    Logger::root(drain, o!())
}

async fn new_instance(
    client: &Client,
    name: String,
    id: Uuid,
    vcpus: u8,
    memory: u64,
    disks: Vec<DiskRequest>,
) -> anyhow::Result<()> {
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

    let request = InstanceEnsureRequest {
        properties,
        // TODO: Allow specifying NICs
        nics: vec![],
        disks,
        migrate: None,
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
    let mut next_raw = false;

    loop {
        tokio::select! {
            c = stdin.read_u8() => {
                match c? {
                    // Ctrl-A means send next one raw
                    b'\x01' if !next_raw => {
                        next_raw = true;
                    }
                    c => {
                        // Exit on non-raw Ctrl-C
                        if c == b'\x03' && !next_raw {
                            break;
                        }

                        ws.send(Message::binary(vec![c])).await?;

                        if next_raw {
                            next_raw = false;
                        }
                    },
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

async fn migrate_instance(
    src_client: Client,
    dst_client: Client,
    src_name: String,
    src_addr: SocketAddr,
    dst_uuid: Uuid,
) -> anyhow::Result<()> {
    // Grab the src instance UUID
    let src_uuid = src_client
        .instance_get_uuid(&src_name)
        .await
        .with_context(|| anyhow!("failed to get src instance UUID"))?;

    // Grab the instance details
    let src_instance = src_client
        .instance_get(src_uuid)
        .await
        .with_context(|| anyhow!("failed to get src instance properties"))?;

    let request = InstanceEnsureRequest {
        properties: InstanceProperties {
            // Use a new ID for the destination instance we're creating
            id: dst_uuid,
            ..src_instance.instance.properties
        },
        // TODO: Handle migrating NICs & disks
        nics: vec![],
        disks: vec![],
        migrate: Some(InstanceMigrateInitiateRequest { src_addr, src_uuid }),
    };

    // Initiate the migration via the destination instance
    let migration_id = dst_client
        .instance_ensure(&request)
        .await?
        .migrate
        .ok_or_else(|| anyhow!("no migrate id on response"))?
        .migration_id;

    // Wait for the migration to complete by polling both source and destination
    // TODO: replace with into_iter method call after edition upgrade
    let handles = IntoIterator::into_iter([
        ("src", src_client, src_uuid),
        ("dst", dst_client, dst_uuid),
    ])
    .map(|(role, client, id)| {
        tokio::spawn(async move {
            loop {
                let state = client
                    .instance_migrate_status(id, migration_id)
                    .await?
                    .state;
                println!("{}({}) migration state={:?}", role, id, state);
                if state == MigrationState::Finish {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    });

    future::join_all(handles).await;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();
    let log = create_logger(&opt);

    let addr = SocketAddr::new(opt.server, opt.port);
    let client = Client::new(addr, log.new(o!()));

    match opt.cmd {
        Command::New { name, uuid, vcpus, memory, crucible_disks } => {
            let disks = if let Some(crucible_disks) = crucible_disks {
                parse_crucible_disks(&crucible_disks)?
            } else {
                vec![]
            };
            new_instance(
                &client,
                name.to_string(),
                uuid.unwrap_or_else(Uuid::new_v4),
                vcpus,
                memory,
                disks,
            )
            .await?
        }
        Command::Get { name } => get_instance(&client, name).await?,
        Command::State { name, state } => {
            put_instance(&client, name, state).await?
        }
        Command::Serial { name } => serial(&client, addr, name).await?,
        Command::Migrate { name, dst_server, dst_port, dst_uuid } => {
            let dst_addr = SocketAddr::new(dst_server, dst_port);
            let dst_client = Client::new(dst_addr, log.clone());
            let dst_uuid = dst_uuid.unwrap_or_else(Uuid::new_v4);
            migrate_instance(client, dst_client, name, addr, dst_uuid).await?
        }
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
        let guard = RawTermiosGuard(fd, termios);
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
