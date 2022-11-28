//! Abstractions for Crucible-backed disks.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};

use crucible_client_types::{CrucibleOpts, VolumeConstructionRequest};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use tracing::{error, info};
use uuid::Uuid;

use crate::{guest_os::GuestOsKind, server_log_mode::ServerLogMode};

use super::BlockSize;

/// An RAII wrapper around a directory containing Crucible data files. Deletes
/// the directory and its contents when dropped.
#[derive(Debug)]
struct DataDirectory {
    path: PathBuf,
}

impl Drop for DataDirectory {
    fn drop(&mut self) {
        info!(?self.path, "Deleting Crucible downstairs data directory");
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            error!(?e, ?self.path, "Failed to delete Crucible downstairs data");
        }
    }
}

/// An RAII wrapper around a Crucible downstairs process. Stops the process and
/// deletes the downstairs' data when dropped.
#[allow(dead_code)]
#[derive(Debug)]
struct Downstairs {
    process_handle: std::process::Child,
    address: SocketAddr,
    data_dir: DataDirectory,
}

impl Drop for Downstairs {
    fn drop(&mut self) {
        info!(?self, "Stopping Crucible downstairs process");
        let _ = self.process_handle.kill();
    }
}

/// An RAII wrapper around a Crucible disk.
#[derive(Debug)]
pub struct CrucibleDisk {
    /// The UUID to insert into this disk's `VolumeConstructionRequest`s.
    id: Uuid,

    /// The disk's block size.
    block_size: BlockSize,

    /// The collection of downstairs process wrappers for this disk.
    downstairs_instances: Vec<Downstairs>,

    /// An optional path to a file to use as a read-only parent for this disk.
    read_only_parent: Option<PathBuf>,

    /// The kind of guest OS that can be found on this disk, if there is one.
    guest_os: Option<GuestOsKind>,

    /// The base64-encoded encryption key to use for this disk.
    encryption_key: String,

    /// The generation number to insert into this disk's
    /// `VolumeConstructionRequest`s.
    generation: AtomicU64,
}

impl CrucibleDisk {
    /// Constructs a new Crucible disk that stores its files in the supplied
    /// `data_dir`.
    pub(crate) fn new(
        disk_size_gib: u64,
        block_size: BlockSize,
        downstairs_binary_path: &impl AsRef<std::ffi::OsStr>,
        downstairs_ports: &[u16],
        data_dir: &impl AsRef<Path>,
        read_only_parent: Option<&impl AsRef<Path>>,
        guest_os: Option<GuestOsKind>,
        log_mode: ServerLogMode,
    ) -> anyhow::Result<Self> {
        // To create a region, Crucible requires a block size, an extent size
        // given as a number of blocks, and an extent count. Compute the latter
        // two figures here. The extent size is semi-arbitrarily chosen to be 64
        // MiB (to match Omicron's extent size at the time this module was
        // authored); this can be parameterized later if needed.
        const EXTENT_SIZE: u64 = 64 << 20;

        assert!(EXTENT_SIZE > block_size.bytes());
        assert!(EXTENT_SIZE % block_size.bytes() == 0);

        let blocks_per_extent: u64 = EXTENT_SIZE / block_size.bytes();
        let extents_in_disk = (disk_size_gib * (1 << 30)) / EXTENT_SIZE;

        // Create the region directories for each region.
        let mut data_dirs = vec![];
        let disk_uuid = Uuid::new_v4();
        for port in downstairs_ports {
            let mut data_dir_path = data_dir.as_ref().to_path_buf();
            data_dir_path.push(format!("{}_{}", disk_uuid, port));
            std::fs::create_dir_all(&data_dir_path)?;
            data_dirs.push(DataDirectory { path: data_dir_path });
        }

        for dir in &data_dirs {
            let dir_arg = dir.path.to_string_lossy();
            let crucible_args = [
                "create",
                "--block-size",
                &block_size.bytes().to_string(),
                "--data",
                dir_arg.as_ref(),
                "--encrypted",
                "true",
                "--uuid",
                &disk_uuid.to_string(),
                "--extent-size",
                &blocks_per_extent.to_string(),
                "--extent-count",
                &extents_in_disk.to_string(),
            ];

            // This is a transient process, so just pipe stdout/stderr back into
            // the framework and trace the outputs instead of setting up full
            // log files.
            let create_stdout = Stdio::piped();
            let create_stderr = Stdio::piped();
            let create_proc = run_crucible_downstairs(
                &downstairs_binary_path,
                &crucible_args,
                create_stdout,
                create_stderr,
            )?;

            let create_output = create_proc.wait_with_output()?;
            info!(
                stdout = std::str::from_utf8(&create_output.stdout)?,
                stderr = std::str::from_utf8(&create_output.stderr)?,
                status = ?create_output.status,
                "Created Crucible region using crucible-downstairs"
            );

            if !create_output.status.success() {
                anyhow::bail!(
                    "Crucible region creation failed with exit code {:?}",
                    create_output.status.code()
                );
            }
        }

        // Spawn the downstairs processes that will serve requests from guest
        // VMs.
        let mut downstairs_instances = vec![];
        for (port, dir) in
            downstairs_ports.into_iter().zip(data_dirs.into_iter())
        {
            let addr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), *port);
            let dir_arg = dir.path.to_string_lossy();
            let crucible_args = [
                "run",
                "--address",
                &addr.ip().to_string(),
                "--port",
                &addr.port().to_string(),
                "--data",
                dir_arg.as_ref(),
            ];

            let (stdout, stderr) = log_mode.get_handles(
                data_dir,
                &format!("crucible_{}_{}", disk_uuid, port),
            )?;

            info!(?crucible_args, "Launching Crucible downstairs server");
            let downstairs = Downstairs {
                process_handle: run_crucible_downstairs(
                    &downstairs_binary_path,
                    &crucible_args,
                    stdout,
                    stderr,
                )?,
                address: SocketAddr::V4(addr),
                data_dir: dir,
            };

            downstairs_instances.push(downstairs);
        }

        Ok(Self {
            id: disk_uuid,
            block_size,
            downstairs_instances,
            read_only_parent: read_only_parent
                .map(|p| p.as_ref().to_path_buf()),
            encryption_key: base64::encode({
                let mut bytes: [u8; 32] = [0; 32];
                StdRng::from_entropy().fill_bytes(&mut bytes);
                bytes
            }),
            guest_os,
            generation: AtomicU64::new(1),
        })
    }

    /// Sets the generation number to use in subsequent calls to create a
    /// backend spec for this disk.
    pub fn set_generation(&self, gen: u64) {
        self.generation.store(gen, Ordering::Relaxed);
    }
}

impl super::DiskConfig for CrucibleDisk {
    fn backend_spec(&self) -> propolis_client::instance_spec::StorageBackend {
        let gen = self.generation.load(Ordering::Relaxed);
        let downstairs_addrs =
            self.downstairs_instances.iter().map(|ds| ds.address).collect();

        propolis_client::instance_spec::StorageBackend {
            kind:
                propolis_client::instance_spec::StorageBackendKind::Crucible {
                    gen,
                    req: VolumeConstructionRequest::Volume {
                        id: self.id,
                        block_size: self.block_size.bytes(),
                        sub_volumes: vec![VolumeConstructionRequest::Region {
                            block_size: self.block_size.bytes(),
                            opts: CrucibleOpts {
                                id: Uuid::new_v4(),
                                target: downstairs_addrs,
                                lossy: false,
                                flush_timeout: None,
                                key: Some(self.encryption_key.clone()),
                                cert_pem: None,
                                key_pem: None,
                                root_cert_pem: None,
                                control: None,
                                read_only: false,
                            },
                            gen,
                        }],
                        read_only_parent: self.read_only_parent.as_ref().map(
                            |p| {
                                Box::new(VolumeConstructionRequest::File {
                                    id: Uuid::new_v4(),
                                    block_size: self.block_size.bytes(),
                                    path: p.to_string_lossy().to_string(),
                                })
                            },
                        ),
                    },
                },
            readonly: false,
        }
    }

    fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }
}

fn run_crucible_downstairs(
    binary_path: &impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    stdout: impl Into<Stdio>,
    stderr: impl Into<Stdio>,
) -> anyhow::Result<std::process::Child> {
    info!(?args, "Running crucible-downstairs");
    let process_handle = std::process::Command::new(&binary_path)
        .args(args)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;

    Ok(process_handle)
}
