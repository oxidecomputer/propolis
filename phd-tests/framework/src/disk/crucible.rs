// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for Crucible-backed disks.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};

use anyhow::Context;
use propolis_client::{
    types::{ComponentV0, CrucibleStorageBackend},
    CrucibleOpts, VolumeConstructionRequest,
};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use tracing::{error, info};
use uuid::Uuid;

use super::BlockSize;
use crate::{disk::DeviceName, guest_os::GuestOsKind, log_config::LogConfig};

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

    /// The address on which this downstairs is serving its API.
    address: SocketAddr,

    /// The address to insert as a connection target when constructing a VCR
    /// that refers to this downstairs. If `None`, the downstairs's API address
    /// is used instead.
    vcr_address_override: Option<SocketAddr>,

    data_dir: DataDirectory,
}

impl Downstairs {
    fn vcr_address(&self) -> SocketAddr {
        self.vcr_address_override.unwrap_or(self.address)
    }
}

impl Drop for Downstairs {
    fn drop(&mut self) {
        info!(?self, "Stopping Crucible downstairs process");
        let _ = self.process_handle.kill();
        let _ = self.process_handle.wait();
    }
}

/// An RAII wrapper around a Crucible disk.
#[derive(Debug)]
pub struct CrucibleDisk {
    device_name: DeviceName,
    disk_id: Uuid,
    guest_os: Option<GuestOsKind>,
    inner: Mutex<Inner>,
}

impl CrucibleDisk {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        device_name: DeviceName,
        min_disk_size_gib: u64,
        block_size: BlockSize,
        downstairs_binary_path: &impl AsRef<std::ffi::OsStr>,
        downstairs_ports: &[u16],
        data_dir_root: &impl AsRef<Path>,
        read_only_parent: Option<&impl AsRef<Path>>,
        guest_os: Option<GuestOsKind>,
        log_config: LogConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            device_name,
            disk_id: Uuid::new_v4(),
            guest_os,
            inner: Mutex::new(Inner::new(
                min_disk_size_gib,
                block_size,
                downstairs_binary_path,
                downstairs_ports,
                data_dir_root,
                read_only_parent,
                log_config,
            )?),
        })
    }

    /// Obtains the current volume construction request for this disk.
    pub fn vcr(&self) -> VolumeConstructionRequest {
        self.inner.lock().unwrap().vcr(self.disk_id)
    }

    /// Sets the generation number to use in subsequent calls to create a
    /// backend spec for this disk.
    pub fn set_generation(&self, generation: u64) {
        self.inner.lock().unwrap().generation = generation;
    }

    /// Changes this disk's downstairs configuration so that the returned IP
    /// address of the first downstairs is an IPv6 black hole instead of its
    /// actual address. This will prevent VMs from activating this disk until
    /// the VCR is replaced with one bearing the correct IP address.
    pub fn enable_vcr_black_hole(&self) {
        info!(disk = self.device_name.as_str(), "enabling vcr black hole");

        // 100::/64 is the IPv6 discard prefix (per RFC 6666).
        let address = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0),
            9000,
            0,
            0,
        ));

        let mut inner = self.inner.lock().unwrap();

        // Crucible rejects VCR replacement requests that change more than one
        // downstairs address, so just invalidate the first downstairs address.
        // This ensures that if the black hole is disabled, subsequent VCRs are
        // valid replacements for the ones produced while the black hole was
        // enabled.
        inner.downstairs_instances[0].vcr_address_override = Some(address);
    }

    /// Ensures that this disk's downstairs configuration will return the
    /// correct addresses for all its downstairs instances.
    pub fn disable_vcr_black_hole(&self) {
        info!(disk = self.device_name.as_str(), "disabling vcr black hole");
        let mut inner = self.inner.lock().unwrap();
        inner.downstairs_instances[0].vcr_address_override = None;
    }
}

impl super::DiskConfig for CrucibleDisk {
    fn device_name(&self) -> &DeviceName {
        &self.device_name
    }

    fn backend_spec(&self) -> ComponentV0 {
        self.inner.lock().unwrap().backend_spec(self.disk_id)
    }

    fn guest_os(&self) -> Option<GuestOsKind> {
        self.guest_os
    }

    fn as_crucible(&self) -> Option<&CrucibleDisk> {
        Some(self)
    }
}

#[derive(Debug)]
struct Inner {
    /// The disk's block size.
    block_size: BlockSize,

    /// The number of blocks in this disk's region's extents.
    blocks_per_extent: u64,

    /// The number of extents in each of the disk's regions.
    extent_count: u32,

    /// The collection of downstairs process wrappers for this disk.
    downstairs_instances: Vec<Downstairs>,

    /// An optional path to a file to use as a read-only parent for this disk.
    read_only_parent: Option<PathBuf>,

    /// The base64-encoded encryption key to use for this disk.
    encryption_key: String,

    /// The generation number to insert into this disk's
    /// `VolumeConstructionRequest`s.
    generation: u64,
}

impl Inner {
    /// Constructs a new Crucible disk that stores its files in the supplied
    /// `data_dir`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        min_disk_size_gib: u64,
        block_size: BlockSize,
        downstairs_binary_path: &impl AsRef<std::ffi::OsStr>,
        downstairs_ports: &[u16],
        data_dir_root: &impl AsRef<Path>,
        read_only_parent: Option<&impl AsRef<Path>>,
        log_config: LogConfig,
    ) -> anyhow::Result<Self> {
        // To create a region, Crucible requires a block size, an extent size
        // given as a number of blocks, and an extent count. Compute the latter
        // two figures here. The extent size is semi-arbitrarily chosen to be 64
        // MiB (to match Omicron's extent size at the time this module was
        // authored); this can be parameterized later if needed.
        const EXTENT_SIZE: u64 = 64 << 20;
        const GIBIBYTE: u64 = 1 << 30;

        assert!(EXTENT_SIZE > block_size.bytes());
        assert!(EXTENT_SIZE % block_size.bytes() == 0);

        let disk_size_gib = match read_only_parent {
            // If there's a read-only parent, ensure the disk is large enough to
            // fit the entire parent, even if its size exceeds the minimum
            // requested disk size.
            Some(parent) => {
                let path = parent.as_ref();
                let meta = std::fs::metadata(path).with_context(|| {
                    format!(
                        "Failed to get fs metadata for read-only parent '{}'",
                        path.display()
                    )
                })?;
                let parent_bytes = meta.len();
                let mut parent_gib = parent_bytes / GIBIBYTE;
                // if the parent's size is not evenly divisible by 1 GiB, add 1
                // GiB to the required size to ensure the parent is not truncated.
                if parent_bytes % GIBIBYTE > 0 {
                    parent_gib += 1;
                }

                if parent_gib > min_disk_size_gib {
                    info!(
                        parent.size_bytes = parent_bytes,
                        parent.size_gib = parent_gib,
                        min.size_gib = min_disk_size_gib,
                        "Increasing minimum disk size to ensure read-only \
                        parent is not truncated",
                    );
                    parent_gib
                } else {
                    min_disk_size_gib
                }
            }
            // If no read-only parent is specified, just use the minimum
            // requested disk size.
            None => min_disk_size_gib,
        };

        let blocks_per_extent: u64 = EXTENT_SIZE / block_size.bytes();
        let extents_in_disk = (disk_size_gib * GIBIBYTE) / EXTENT_SIZE;

        // Create the region directories for each region.
        let mut data_dirs = vec![];
        let disk_uuid = Uuid::new_v4();
        for port in downstairs_ports {
            let mut data_dir_path = data_dir_root.as_ref().to_path_buf();
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
        for (port, dir) in downstairs_ports.iter().zip(data_dirs.into_iter()) {
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

            // NOTE: `log_format` is ignored here because Crucible determines
            // Bunyan or plain formatting based on `atty::is()`. In practice
            // this is fine, and matches what we want right now, but it might be
            // nice to connect this more directly to the output desire expressed
            // by the test runner.
            let (stdout, stderr) = log_config.output_mode.get_handles(
                data_dir_root,
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
                vcr_address_override: None,
                data_dir: dir,
            };

            downstairs_instances.push(downstairs);
        }

        Ok(Self {
            block_size,
            blocks_per_extent,
            extent_count: extents_in_disk as u32,
            downstairs_instances,
            read_only_parent: read_only_parent
                .map(|p| p.as_ref().to_path_buf()),
            encryption_key: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                {
                    let mut bytes: [u8; 32] = [0; 32];
                    StdRng::from_entropy().fill_bytes(&mut bytes);
                    bytes
                },
            ),
            generation: 1,
        })
    }

    fn backend_spec(&self, disk_id: Uuid) -> ComponentV0 {
        let vcr = self.vcr(disk_id);

        ComponentV0::CrucibleStorageBackend(CrucibleStorageBackend {
            request_json: serde_json::to_string(&vcr)
                .expect("VolumeConstructionRequest should serialize"),
            readonly: false,
        })
    }

    fn vcr(&self, disk_id: Uuid) -> VolumeConstructionRequest {
        let downstairs_addrs = self
            .downstairs_instances
            .iter()
            .map(Downstairs::vcr_address)
            .collect();

        VolumeConstructionRequest::Volume {
            id: disk_id,
            block_size: self.block_size.bytes(),
            sub_volumes: vec![VolumeConstructionRequest::Region {
                block_size: self.block_size.bytes(),
                blocks_per_extent: self.blocks_per_extent,
                extent_count: self.extent_count,
                opts: CrucibleOpts {
                    id: disk_id,
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
                r#gen: self.generation,
            }],
            read_only_parent: self.read_only_parent.as_ref().map(|p| {
                Box::new(VolumeConstructionRequest::File {
                    id: Uuid::new_v4(),
                    block_size: self.block_size.bytes(),
                    path: p.to_string_lossy().to_string(),
                })
            }),
        }
    }
}

fn run_crucible_downstairs(
    binary_path: &impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    stdout: impl Into<Stdio>,
    stderr: impl Into<Stdio>,
) -> anyhow::Result<std::process::Child> {
    info!(?args, "Running crucible-downstairs");
    let process_handle = std::process::Command::new(binary_path)
        .args(args)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;

    Ok(process_handle)
}
