// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::prelude::AsRawFd,
    time::Duration,
};

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use futures::{future, SinkExt};
use newtype_uuid::{GenericUuid, TypedUuid, TypedUuidKind, TypedUuidTag};
use propolis_client::types::InstanceMetadata;
use slog::{o, Drain, Level, Logger};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};
use uuid::Uuid;

use propolis_client::{
    support::{InstanceSerialConsoleHelper, WSClientOffset},
    types::{
        DiskRequest, InstanceEnsureRequest, InstanceMigrateInitiateRequest,
        InstanceProperties, InstanceStateRequested, InstanceVcrReplace,
        MigrationState,
    },
    Client,
};

#[derive(Debug, Parser)]
#[clap(about, version)]
/// A simple CLI tool to manipulate propolis-server
struct Opt {
    /// propolis-server address
    #[clap(short, long, value_parser = resolve_host)]
    server: IpAddr,

    /// propolis-server port
    #[clap(short, long, default_value = "12400", action)]
    port: u16,

    /// Enable debugging
    #[clap(short, long, action)]
    debug: bool,

    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new propolis instance
    New {
        /// Instance name
        #[clap(action)]
        name: String,

        /// Instance uuid (if specified)
        #[clap(short = 'u', action)]
        uuid: Option<Uuid>,

        /// Number of vCPUs allocated to instance
        #[clap(short = 'c', default_value = "4", action)]
        vcpus: u8,

        /// Memory allocated to instance (MiB)
        #[clap(short, default_value = "1024", action)]
        memory: u64,

        /// File with a JSON array of DiskRequest structs
        #[clap(long, action)]
        crucible_disks: Option<PathBuf>,

        // cloud_init ISO file
        #[clap(long, action)]
        cloud_init: Option<PathBuf>,

        /// A UUID to use for the instance's silo, attached to instance metrics.
        #[clap(long)]
        silo_id: Option<TypedUuid<SiloKind>>,

        /// A UUID to use for the instance's project, attached to instance metrics.
        #[clap(long)]
        project_id: Option<TypedUuid<ProjectKind>>,

        /// A UUID to use for the instance's hosting sled, attached to instance
        /// metrics.
        #[clap(long)]
        sled_id: Option<TypedUuid<SledKind>>,

        /// A model number to use for the instance's hosting sled, attached to
        /// instance metrics.
        #[clap(long, default_value_t = String::from("fake-gimlet"))]
        sled_model: String,

        /// A revision number to use for the instance's hosting sled, attached to
        /// instance metrics.
        #[clap(long, default_value_t = 1)]
        sled_revision: u32,

        /// A serial number to use for the instance's hosting sled, attached to
        /// instance metrics.
        #[clap(long, default_value_t = String::from("fake-serial"))]
        sled_serial: String,
    },

    /// Get the properties of a propolis instance
    Get,

    /// Transition the instance to a new state
    State {
        /// The requested state
        #[clap(value_parser = parse_state)]
        state: InstanceStateRequested,
    },

    /// Drop to a Serial console connected to the instance
    Serial {
        /// The offset since boot (or if negative, the current end of the
        /// buffered data) from which to retrieve output history.
        /// Defaults to the most recent 16 KiB of console output (-16384).
        #[clap(long, short)]
        byte_offset: Option<i64>,
    },

    /// Migrate instance to new propolis-server
    Migrate {
        /// Destination propolis-server address
        #[clap(value_parser = resolve_host)]
        dst_server: IpAddr,

        /// Destination propolis-server port
        #[clap(short = 'p', default_value = "12400", action)]
        dst_port: u16,

        /// Uuid for the destination instance
        #[clap(short = 'u', action)]
        dst_uuid: Option<Uuid>,

        /// File with a JSON array of DiskRequest structs
        #[clap(long, action)]
        crucible_disks: Option<PathBuf>,
    },

    /// Monitor an instance's state in real time
    Monitor,

    /// Inject an NMI into the instance
    InjectNmi,

    /// Call the VolumeConstructionRequest replace endpoint
    Vcr {
        /// Uuid for the disk
        #[clap(short = 'u', action)]
        uuid: Uuid,

        /// File with a JSON InstanceVcrReplace struct
        #[clap(long, action)]
        vcr_replace: PathBuf,
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

fn parse_json_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> anyhow::Result<T> {
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

// Implement typed UUID wrappers for the project / silo IDs, to avoid conflating
// them.
enum ProjectKind {}

impl TypedUuidKind for ProjectKind {
    fn tag() -> TypedUuidTag {
        const TAG: TypedUuidTag = TypedUuidTag::new("project");
        TAG
    }
}

enum SiloKind {}

impl TypedUuidKind for SiloKind {
    fn tag() -> TypedUuidTag {
        const TAG: TypedUuidTag = TypedUuidTag::new("silo");
        TAG
    }
}

enum SledKind {}

impl TypedUuidKind for SledKind {
    fn tag() -> TypedUuidTag {
        const TAG: TypedUuidTag = TypedUuidTag::new("sled");
        TAG
    }
}

#[allow(clippy::too_many_arguments)]
async fn new_instance(
    client: &Client,
    name: String,
    id: Uuid,
    vcpus: u8,
    memory: u64,
    disks: Vec<DiskRequest>,
    cloud_init_bytes: Option<String>,
    metadata: InstanceMetadata,
) -> anyhow::Result<()> {
    let properties = InstanceProperties {
        id,
        name,
        description: "propolis-cli generated instance".to_string(),
        metadata,
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
        cloud_init_bytes,
    };

    // Try to create the instance
    client
        .instance_ensure()
        .body(request)
        .send()
        .await
        .with_context(|| anyhow!("failed to create instance"))?;

    Ok(())
}

async fn replace_vcr(
    client: &Client,
    id: Uuid,
    vcr_replace: InstanceVcrReplace,
) -> anyhow::Result<()> {
    // Try to call the endpoint
    client
        .instance_issue_crucible_vcr_request()
        .id(id)
        .body(vcr_replace)
        .send()
        .await
        .with_context(|| anyhow!("failed to issue vcr request"))?;

    Ok(())
}

async fn get_instance(client: &Client) -> anyhow::Result<()> {
    let res = client
        .instance_get()
        .send()
        .await
        .with_context(|| anyhow!("failed to get instance properties"))?;

    println!("{:#?}", res.instance);

    Ok(())
}

async fn put_instance(
    client: &Client,
    state: InstanceStateRequested,
) -> anyhow::Result<()> {
    client
        .instance_state_put()
        .body(state)
        .send()
        .await
        .with_context(|| anyhow!("failed to set instance state"))?;

    Ok(())
}

async fn stdin_to_websockets_task(
    mut stdinrx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    wstx: tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    // next_raw must live outside loop, because Ctrl-A should work across
    // multiple inbuf reads.
    let mut next_raw = false;

    loop {
        let inbuf = if let Some(inbuf) = stdinrx.recv().await {
            inbuf
        } else {
            continue;
        };

        // Put bytes from inbuf to outbuf, but don't send Ctrl-A unless
        // next_raw is true.
        let mut outbuf = Vec::with_capacity(inbuf.len());

        let mut exit = false;
        for c in inbuf {
            match c {
                // Ctrl-A means send next one raw
                b'\x01' => {
                    if next_raw {
                        // Ctrl-A Ctrl-A should be sent as Ctrl-A
                        outbuf.push(c);
                        next_raw = false;
                    } else {
                        next_raw = true;
                    }
                }
                b'\x03' => {
                    if !next_raw {
                        // Exit on non-raw Ctrl-C
                        exit = true;
                        break;
                    } else {
                        // Otherwise send Ctrl-C
                        outbuf.push(c);
                        next_raw = false;
                    }
                }
                _ => {
                    outbuf.push(c);
                    next_raw = false;
                }
            }
        }

        // Send what we have, even if there's a Ctrl-C at the end.
        if !outbuf.is_empty() {
            wstx.send(outbuf).await.unwrap();
        }

        if exit {
            break;
        }
    }
}

async fn serial(
    addr: SocketAddr,
    byte_offset: Option<i64>,
    log: Logger,
) -> anyhow::Result<()> {
    let mut ws_console = serial_connect(addr, byte_offset, log).await?;

    let _raw_guard = RawTermiosGuard::stdio_guard()
        .with_context(|| anyhow!("failed to set raw mode"))?;

    let mut stdout = tokio::io::stdout();

    // https://docs.rs/tokio/latest/tokio/io/trait.AsyncReadExt.html#method.read_exact
    // is not cancel safe! Meaning reads from tokio::io::stdin are not cancel
    // safe. Spawn a separate task to read and put bytes onto this channel.
    let (stdintx, stdinrx) = tokio::sync::mpsc::channel(16);
    let (wstx, mut wsrx) = tokio::sync::mpsc::channel(16);

    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut inbuf = [0u8; 1024];

        loop {
            let n = match stdin.read(&mut inbuf).await {
                Err(_) | Ok(0) => break,
                Ok(n) => n,
            };

            stdintx.send(inbuf[0..n].to_vec()).await.unwrap();
        }
    });

    tokio::spawn(async move { stdin_to_websockets_task(stdinrx, wstx).await });

    loop {
        tokio::select! {
            c = wsrx.recv() => {
                match c {
                    None => {
                        // channel is closed
                        break;
                    }
                    Some(c) => {
                        ws_console.send(Message::Binary(c)).await?;
                    },
                }
            }
            msg = ws_console.recv() => {
                match msg {
                    Some(Ok(msg)) => {
                        match msg.process().await {
                            Ok(Message::Binary(input)) => {
                                stdout.write_all(&input).await?;
                                stdout.flush().await?;
                            }
                            Ok(Message::Close(Some(CloseFrame {code, reason}))) => {
                                eprint!("\r\nConnection closed: {:?}\r\n", code);
                                match code {
                                    CloseCode::Abnormal
                                    | CloseCode::Error
                                    | CloseCode::Extension
                                    | CloseCode::Invalid
                                    | CloseCode::Policy
                                    | CloseCode::Protocol
                                    | CloseCode::Size
                                    | CloseCode::Unsupported => {
                                        anyhow::bail!("{}", reason);
                                    }
                                    _ => break,
                                }
                            }
                            Ok(Message::Close(None)) => {
                                eprint!("\r\nConnection closed.\r\n");
                                break;
                            }
                            // note: migration events via Message::Text are
                            // already handled within ws_console.recv(), but
                            // would still be available to match here if we want
                            // to indicate that it happened to the user
                            _ => continue,
                        }
                    }
                    None => {
                        eprint!("\r\nConnection lost.\r\n");
                        break;
                    }
                    _ => continue,
                }
            }
        }
    }

    Ok(())
}

async fn serial_connect(
    addr: SocketAddr,
    byte_offset: Option<i64>,
    log: Logger,
) -> anyhow::Result<InstanceSerialConsoleHelper> {
    let offset = match byte_offset {
        Some(x) if x >= 0 => WSClientOffset::FromStart(x as u64),
        Some(x) => WSClientOffset::MostRecent(-x as u64),
        None => WSClientOffset::MostRecent(16384),
    };

    Ok(InstanceSerialConsoleHelper::new(addr, offset, Some(log)).await?)
}

async fn migrate_instance(
    src_client: Client,
    dst_client: Client,
    src_addr: SocketAddr,
    dst_uuid: Uuid,
    disks: Vec<DiskRequest>,
) -> anyhow::Result<()> {
    // Grab the instance details
    let src_instance =
        src_client.instance_get().send().await.with_context(|| {
            anyhow!("failed to get src instance properties")
        })?;
    let src_uuid = src_instance.instance.properties.id;

    let request = InstanceEnsureRequest {
        properties: InstanceProperties {
            // Use a new ID for the destination instance we're creating
            id: dst_uuid,
            ..src_instance.instance.properties.clone()
        },
        // TODO: Handle migrating NICs
        nics: vec![],
        disks,
        migrate: Some(InstanceMigrateInitiateRequest {
            migration_id: Uuid::new_v4(),
            src_addr: src_addr.to_string(),
            src_uuid,
        }),
        cloud_init_bytes: None,
    };

    // Initiate the migration via the destination instance
    let migration_res =
        dst_client.instance_ensure().body(request).send().await?;
    let migration_id = migration_res
        .migrate
        .as_ref()
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
                let state =
                    client.instance_migrate_status().send().await?.into_inner();

                let migration = if role == "src" {
                    state.migration_out
                } else {
                    state.migration_in
                };

                // The destination should start reporting migration status as
                // soon as the ensure request completes. The source may not
                // have a migration status yet because the request from the
                // destination needs to arrive first.
                let Some(migration) = migration else {
                    if role == "dst" {
                        anyhow::bail!("dst instance's migration ID wasn't set");
                    } else {
                        println!("src hasn't received migration request yet");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                if migration.id != migration_id {
                    anyhow::bail!(
                        "{role} instance's migration ID is wrong: \
                                  got {}, expected {migration_id}",
                        migration.id
                    );
                }

                let state = migration.state;
                println!("{}({}) migration state={:?}", role, id, state);
                if state == MigrationState::Finish {
                    return Ok::<_, anyhow::Error>(());
                } else if state == MigrationState::Error {
                    return Err(anyhow::anyhow!(
                        "{role} instance ran into error during migration"
                    ));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    });

    future::join_all(handles)
        .await
        // Hoist out any JoinErrors
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        // Then any errors from polling the source/destination
        .into_iter()
        .collect::<anyhow::Result<()>>()?;

    Ok(())
}

async fn monitor(addr: SocketAddr) -> anyhow::Result<()> {
    // We use a custom client builder here because the default progenitor
    // one has a timeout of 15s but we want to be able to wait indefinitely.
    let client = reqwest::ClientBuilder::new().build().unwrap();
    let client = propolis_client::Client::new_with_client(
        &format!("http://{addr}"),
        client,
    );
    let mut gen = 0;
    loop {
        // State monitoring always returns the most recent state/gen pair
        // known to Propolis.
        let response = client
            .instance_state_monitor()
            .body(propolis_client::types::InstanceStateMonitorRequest { gen })
            .send()
            .await
            .with_context(|| anyhow!("failed to get new instance state"))?;

        println!("InstanceState: {:?}", response.state);

        if response.state == propolis_client::types::InstanceState::Destroyed {
            return Ok(());
        }

        // Update the generation number we're asking for, to ensure the
        // Propolis will only return more recent values.
        gen = response.gen + 1;
    }
}

async fn inject_nmi(client: &Client) -> anyhow::Result<()> {
    client
        .instance_issue_nmi()
        .send()
        .await
        .with_context(|| anyhow!("failed to inject NMI"))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();
    let log = create_logger(&opt);

    let addr = SocketAddr::new(opt.server, opt.port);
    let client = Client::new(&format!("http://{addr}"));

    match opt.cmd {
        Command::New {
            name,
            uuid,
            vcpus,
            memory,
            crucible_disks,
            cloud_init,
            silo_id,
            project_id,
            sled_id,
            sled_model,
            sled_revision,
            sled_serial,
        } => {
            let disks = if let Some(crucible_disks) = crucible_disks {
                parse_json_file(&crucible_disks)?
            } else {
                vec![]
            };
            let cloud_init_bytes = if let Some(cloud_init) = cloud_init {
                Some(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    std::fs::read(cloud_init)?,
                ))
            } else {
                None
            };
            let metadata = InstanceMetadata {
                project_id: project_id
                    .unwrap_or_else(TypedUuid::new_v4)
                    .into_untyped_uuid(),
                silo_id: silo_id
                    .unwrap_or_else(TypedUuid::new_v4)
                    .into_untyped_uuid(),
                sled_id: sled_id
                    .unwrap_or_else(TypedUuid::new_v4)
                    .into_untyped_uuid(),
                sled_model,
                sled_revision,
                sled_serial,
            };
            new_instance(
                &client,
                name.to_string(),
                uuid.unwrap_or_else(Uuid::new_v4),
                vcpus,
                memory,
                disks,
                cloud_init_bytes,
                metadata,
            )
            .await?
        }
        Command::Get => get_instance(&client).await?,
        Command::State { state } => put_instance(&client, state).await?,
        Command::Serial { byte_offset } => {
            serial(addr, byte_offset, log).await?
        }
        Command::Migrate { dst_server, dst_port, dst_uuid, crucible_disks } => {
            let dst_addr = SocketAddr::new(dst_server, dst_port);
            let dst_client = Client::new(&format!("http://{dst_addr}"));
            let dst_uuid = dst_uuid.unwrap_or_else(Uuid::new_v4);
            let disks = if let Some(crucible_disks) = crucible_disks {
                parse_json_file(&crucible_disks)?
            } else {
                vec![]
            };
            migrate_instance(client, dst_client, addr, dst_uuid, disks).await?
        }
        Command::Monitor => monitor(addr).await?,
        Command::InjectNmi => inject_nmi(&client).await?,
        Command::Vcr { uuid, vcr_replace } => {
            let replace: InstanceVcrReplace = parse_json_file(&vcr_replace)?;
            replace_vcr(&client, uuid, replace).await?
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
            panic!("{:?}", std::io::Error::last_os_error());
        }
    }
}

#[cfg(test)]
mod test {
    use super::stdin_to_websockets_task;

    #[tokio::test]
    async fn test_stdin_to_websockets_task() {
        use tokio::sync::mpsc::error::TryRecvError;

        let (stdintx, stdinrx) = tokio::sync::mpsc::channel(16);
        let (wstx, mut wsrx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(
            async move { stdin_to_websockets_task(stdinrx, wstx).await },
        );

        // send characters, receive characters
        stdintx
            .send("test post please ignore".chars().map(|c| c as u8).collect())
            .await
            .unwrap();
        let actual = wsrx.recv().await.unwrap();
        assert_eq!(
            String::from_utf8(actual).unwrap(),
            "test post please ignore"
        );

        // don't send ctrl-a
        stdintx.send("\x01".chars().map(|c| c as u8).collect()).await.unwrap();
        assert_eq!(wsrx.try_recv(), Err(TryRecvError::Empty));

        // the "t" here is sent "raw" because of last ctrl-a but that doesn't change anything
        stdintx.send("test".chars().map(|c| c as u8).collect()).await.unwrap();
        let actual = wsrx.recv().await.unwrap();
        assert_eq!(String::from_utf8(actual).unwrap(), "test");

        // ctrl-a ctrl-c = only ctrl-c sent
        stdintx
            .send("\x01\x03".chars().map(|c| c as u8).collect())
            .await
            .unwrap();
        let actual = wsrx.recv().await.unwrap();
        assert_eq!(String::from_utf8(actual).unwrap(), "\x03");

        // same as above, across two messages
        stdintx.send("\x01".chars().map(|c| c as u8).collect()).await.unwrap();
        stdintx.send("\x03".chars().map(|c| c as u8).collect()).await.unwrap();
        assert_eq!(wsrx.try_recv(), Err(TryRecvError::Empty));
        let actual = wsrx.recv().await.unwrap();
        assert_eq!(String::from_utf8(actual).unwrap(), "\x03");

        // ctrl-a ctrl-a = only ctrl-a sent
        stdintx
            .send("\x01\x01".chars().map(|c| c as u8).collect())
            .await
            .unwrap();
        let actual = wsrx.recv().await.unwrap();
        assert_eq!(String::from_utf8(actual).unwrap(), "\x01");

        // ctrl-c on its own means exit
        stdintx.send("\x03".chars().map(|c| c as u8).collect()).await.unwrap();
        assert_eq!(wsrx.try_recv(), Err(TryRecvError::Empty));

        // channel is closed
        assert!(wsrx.recv().await.is_none());
    }
}
