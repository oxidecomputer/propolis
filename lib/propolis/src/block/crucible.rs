// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implement a virtual block device backed by Crucible

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::DeviceInfo;
use crate::block;
use crate::inventory::Entity;
use crate::vmm::MemCtx;

use crucible::{BlockIO, Buffer, CrucibleError, SnapshotDetails, Volume};
use crucible_client_types::VolumeConstructionRequest;
use oximeter::types::ProducerRegistry;
use slog::{error, info};
use thiserror::Error;
use uuid::Uuid;

pub use nexus_client::Client as NexusClient;

pub struct CrucibleBackend {
    tokio_rt: tokio::runtime::Handle,
    block_io: Arc<Volume>,
    scheduler: block::Scheduler,

    read_only: bool,
    block_size: u64,
    sectors: u64,

    task_count: NonZeroUsize,
}

/// If [CreateOptions] does not specify a `task_count`, this value will be used.
pub const DEFAULT_TASK_COUNT: NonZeroUsize =
    unsafe { NonZeroUsize::new_unchecked(64) };

pub struct CreateOptions {
    pub request: VolumeConstructionRequest,
    pub read_only: bool,
    pub producer_registry: Option<ProducerRegistry>,
    pub nexus_client: Option<NexusClient>,

    /// Number of worker tasks to spawn for this backend
    ///
    /// This is an upper bound on the number of in-flight IOs possible, since
    /// each task can handle on IO at any given time.  If [None] is specified,
    /// then [DEFAULT_TASK_COUNT] will be used.
    pub task_count: Option<NonZeroUsize>,
}

impl CrucibleBackend {
    pub fn create(
        opts: CreateOptions,
        log: slog::Logger,
    ) -> io::Result<Arc<Self>> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move { CrucibleBackend::_create(opts, log).await })
            .map_err(CrucibleError::into)
    }

    async fn _create(
        opts: CreateOptions,
        log: slog::Logger,
    ) -> Result<Arc<Self>, crucible::CrucibleError> {
        // Construct the volume.
        let volume = Volume::construct(
            opts.request,
            opts.producer_registry,
            Some(log.clone()),
        )
        .await?;

        // Decide if we need to scrub this volume or not.
        if volume.has_read_only_parent() {
            let vclone = volume.clone();
            let nexus_client = opts.nexus_client;
            tokio::spawn(async move {
                let volume_id = vclone.get_uuid().await.unwrap();

                // This does the actual scrub.
                match vclone.scrub(&log, Some(120), Some(25)).await {
                    Ok(()) => {
                        if let Some(nexus_client) = nexus_client {
                            info!(
                                log,
                                "Scrub of volume {} completed, remove parent",
                                volume_id
                            );

                            Self::remove_read_only_parent(
                                &volume_id,
                                nexus_client,
                                log,
                            )
                            .await;
                        } else {
                            // No nexus contact was provided, so just log
                            // a message.
                            info!(
                                log,
                                "Scrub of volume {} completed", volume_id
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            log,
                            "Scrub of volume {} failed: {}", volume_id, e
                        );
                        // TODO: Report error to nexus that scrub failed
                    }
                }
            });
        }

        // After active negotiation, set sizes
        let block_size = volume.get_block_size().await?;
        let total_size = volume.total_size().await?;
        let sectors = total_size / block_size;

        let task_count = opts.task_count.unwrap_or(DEFAULT_TASK_COUNT);

        Ok(Arc::new(Self {
            tokio_rt: tokio::runtime::Handle::current(),
            block_io: Arc::new(volume),
            scheduler: block::Scheduler::new(),

            read_only: opts.read_only,
            block_size,
            sectors,

            task_count,
        }))
    }

    // Communicate to Nexus that we can remove the read only parent for
    // the given volume id.
    async fn remove_read_only_parent(
        volume_id: &Uuid,
        nexus_client: NexusClient,
        log: slog::Logger,
    ) {
        // Notify Nexus of the state change.
        match nexus_client.cpapi_disk_remove_read_only_parent(&volume_id).await
        {
            Ok(_) => {
                info!(
                    log,
                    "Submitted removal for read only parent on {}", volume_id,
                );
            }
            Err(e) => {
                // We finished the scrub, but can't tell Nexus to remove
                // the read only parent. While this is not ideal, as it
                // means we will re-do a scrub the next time this
                // volume is attached, it won't result in any harm to
                // the volume or data.
                error!(log, "Failed removal of read only parent: {}", e,);
            }
        }
    }

    /// Retrieve the UUID identifying this Crucible backend.
    pub fn get_uuid(&self) -> io::Result<uuid::Uuid> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async { self.block_io.get_uuid().await })
            .map_err(CrucibleError::into)
    }

    /// Issue a snapshot request
    pub async fn snapshot(&self, snapshot_id: Uuid) -> io::Result<()> {
        self.block_io
            .flush(Some(SnapshotDetails {
                snapshot_name: snapshot_id.to_string(),
            }))
            .await
            .map_err(CrucibleError::into)
    }
}

impl block::Backend for CrucibleBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors,
            writable: !self.read_only,
        }
    }

    fn process(&self, _req: &block::Request, _mem: &MemCtx) -> block::Result {
        panic!("request dispatch expected to be done through async logic");
    }

    fn attach(&self, dev: Arc<dyn block::Device>) -> io::Result<()> {
        self.scheduler.attach(dev);

        for _n in 0..self.task_count.get() {
            let bdev = self.block_io.clone();
            let ro = self.read_only;
            let mut worker = self.scheduler.worker();
            tokio::spawn(async move {
                loop {
                    let res = match worker.next().await {
                        None => break,
                        Some((req, mguard)) => {
                            match process_request(
                                bdev.as_ref(),
                                ro,
                                req,
                                &mguard,
                            )
                            .await
                            {
                                Ok(_) => block::Result::Success,
                                Err(_) => block::Result::Failure,
                            }
                        }
                    };
                    worker.complete(res);
                }
            });
        }

        Ok(())
    }
}

impl Entity for CrucibleBackend {
    fn type_name(&self) -> &'static str {
        "block-crucible"
    }
    fn start(&self) -> anyhow::Result<()> {
        self.tokio_rt
            .block_on(async move { self.block_io.activate().await })?;

        self.scheduler.start();
        Ok(())
    }
    fn pause(&self) {
        self.scheduler.pause();
    }
    fn resume(&self) {
        self.scheduler.resume();
    }
    fn halt(&self) {
        self.scheduler.halt();
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid guest memory region")]
    BadGuestRegion,
    #[error("backend is read-only")]
    ReadOnly,

    #[error("copied length {0} did not match expectation {1}")]
    CopyError(usize, usize),

    #[error("IO Error")]
    Io(#[from] io::Error),

    #[error("Crucible Error: {0}")]
    Crucible(#[from] CrucibleError),
}

async fn process_request(
    block: &(dyn BlockIO + Send + Sync),
    read_only: bool,
    req: &block::Request,
    mem: &MemCtx,
) -> Result<(), Error> {
    match req.oper() {
        block::Operation::Read(off) => {
            let maps =
                req.mappings(mem).ok_or_else(|| Error::BadGuestRegion)?;

            let len = req.len();
            let offset = block.byte_offset_to_block(off as u64).await?;

            // Perform one large read from crucible, and write from data into
            // mappings
            let data = Buffer::new(len);
            let _ = block.read(offset, data.clone()).await?;

            let source = data.as_vec().await;
            let mut nwritten = 0;
            for mapping in maps {
                nwritten += mapping.write_bytes(
                    &source[nwritten..(nwritten + mapping.len())],
                )?;
            }

            if nwritten != len {
                return Err(Error::CopyError(nwritten, len));
            }
        }
        block::Operation::Write(off) => {
            if read_only {
                return Err(Error::ReadOnly);
            }

            // Read from all the mappings into vec, and perform one large write
            // to crucible
            let maps =
                req.mappings(mem).ok_or_else(|| Error::BadGuestRegion)?;
            let len = req.len();
            let mut vec: Vec<u8> = vec![0; len];
            let mut nread = 0;
            for mapping in maps {
                nread += mapping
                    .read_bytes(&mut vec[nread..(nread + mapping.len())])?;
            }
            if nread != len {
                return Err(Error::CopyError(nread, len));
            }

            let offset = block.byte_offset_to_block(off as u64).await?;
            let _ = block.write(offset, crucible::Bytes::from(vec)).await?;
        }
        block::Operation::Flush(_off, _len) => {
            // Send flush to crucible
            let _ = block.flush(None).await?;
        }
    }
    Ok(())
}
