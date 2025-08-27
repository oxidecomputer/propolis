// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implement a virtual block device backed by Crucible

use std::io;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::Arc;

use crate::block;
use crate::tasks::TaskGroup;
use crate::vmm::MemCtx;

use crucible::{
    BlockIO, Buffer, CrucibleError, ReplaceResult, SnapshotDetails, Volume,
    VolumeBuilder,
};
use crucible_client_types::VolumeConstructionRequest;
use oximeter::types::ProducerRegistry;
use slog::{error, info};
use thiserror::Error;
use uuid::Uuid;

pub use nexus_client::Client as NexusClient;

// TODO: Make this a runtime tunable?
const WORKER_COUNT: NonZeroUsize = NonZeroUsize::new(8).unwrap();

pub struct CrucibleBackend {
    block_attach: block::BackendAttachment,
    state: Arc<WorkerState>,
    workers: TaskGroup,
}
struct WorkerState {
    volume: Volume,
    info: block::DeviceInfo,
    skip_flush: bool,
}
impl WorkerState {
    async fn process_loop(&self, wctx: block::AsyncWorkerCtx) {
        // Start with a read buffer of a single block
        // It will be resized larger (and remain so) if subsequent read
        // operations required additional space.
        let mut readbuf = Buffer::new(1, self.info.block_size as usize);
        loop {
            let Some(dreq) = wctx.wait_for_req().await else {
                break;
            };

            let Some(memctx) = wctx.acc_mem().access() else {
                dreq.complete(block::Result::Failure);
                continue;
            };

            let res = match self
                .process_request(
                    self.volume.deref(),
                    dreq.req(),
                    &mut readbuf,
                    &memctx,
                )
                .await
            {
                Ok(_) => block::Result::Success,
                Err(e) => {
                    let mapped = block::Result::from(e);
                    assert!(mapped.is_err());
                    mapped
                }
            };

            dreq.complete(res);
        }
    }

    async fn process_request(
        &self,
        block: &(dyn BlockIO + Send + Sync),
        req: &block::Request,
        readbuf: &mut Buffer,
        mem: &MemCtx,
    ) -> Result<(), Error> {
        let block_size = self.info.block_size as usize;

        match req.op {
            block::Operation::Read(off, len) => {
                let (off_blocks, len_blocks) =
                    block_offset_count(off, len, block_size)?;

                let maps =
                    req.mappings(mem).ok_or_else(|| Error::BadGuestRegion)?;

                // Perform one large read from crucible, and write from data into
                // mappings
                readbuf.reset(len_blocks, block_size);
                let _ = block.read(off_blocks, readbuf).await?;

                let mut nwritten = 0;
                for mapping in maps {
                    nwritten += mapping.write_bytes(
                        &readbuf[nwritten..(nwritten + mapping.len())],
                    )?;
                }

                if nwritten != len {
                    return Err(Error::CopyError(nwritten, len));
                }
            }
            block::Operation::Write(off, len) => {
                if self.info.read_only {
                    return Err(Error::ReadOnly);
                }

                let (off_blocks, _len_blocks) =
                    block_offset_count(off, len, block_size)?;

                // Read from all the mappings into vec, and perform one large write
                // to crucible
                let maps =
                    req.mappings(mem).ok_or_else(|| Error::BadGuestRegion)?;
                let mut data = crucible::BytesMut::with_capacity(len);
                let mut nread = 0;
                for mapping in maps {
                    let n = mapping.read_bytes_uninit(
                        &mut data.spare_capacity_mut()[..mapping.len()],
                    )?;
                    // `read_bytes` returns the number of bytes written, so we can
                    // expand our initialized area by this amount.
                    unsafe {
                        data.set_len(data.len() + n);
                    }
                    nread += n;
                }
                if nread != len {
                    return Err(Error::CopyError(nread, len));
                }

                let _ = block.write(off_blocks, data).await?;
            }
            block::Operation::Flush => {
                if !self.skip_flush {
                    // Send flush to crucible
                    let _ = block.flush(None).await?;
                }
            }
            block::Operation::Discard(..) => {
                // Crucible does not support discard operations for now
                return Err(Error::Unsupported);
            }
        }
        Ok(())
    }
}

impl CrucibleBackend {
    pub async fn create(
        request: VolumeConstructionRequest,
        opts: block::BackendOpts,
        producer_registry: Option<ProducerRegistry>,
        nexus_client: Option<NexusClient>,
        log: slog::Logger,
    ) -> io::Result<Arc<Self>> {
        // Construct the volume.
        let volume = Volume::construct(request, producer_registry, log.clone())
            .await
            .map_err(|e| io::Error::from(CrucibleError::from(e)))?;

        // Decide if we need to scrub this volume or not.
        if volume.has_read_only_parent() {
            let vclone = volume.clone();
            tokio::spawn(async move {
                let volume_id = vclone.get_uuid().await.unwrap();

                // This does the actual scrub.
                match vclone.scrub(Some(120), Some(25)).await {
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

        let info = block::DeviceInfo {
            block_size: block_size as u32,
            total_size: sectors,
            read_only: opts.read_only.unwrap_or(false),
            supports_discard: false,
        };

        Ok(Arc::new(Self {
            block_attach: block::BackendAttachment::new(WORKER_COUNT, info),
            state: Arc::new(WorkerState {
                volume,
                info,
                skip_flush: opts.skip_flush.unwrap_or(false),
            }),
            workers: TaskGroup::new(),
        }))
    }

    /// Return the block size of this Crucible backend, if it can be determined.
    pub async fn block_size(&self) -> Option<u32> {
        self.state
            .volume
            .get_block_size()
            .await
            .ok()
            .and_then(|sz| sz.try_into().ok())
    }

    /// Create Crucible backend using the in-memory volume backend, rather than
    /// "real" Crucible downstairs instances.
    pub async fn create_mem(
        size: u64,
        opts: block::BackendOpts,
        log: slog::Logger,
    ) -> io::Result<Arc<Self>> {
        let block_size = u64::from(opts.block_size.ok_or_else(|| {
            CrucibleError::GenericError(
                "block_size is required parameter".into(),
            )
        })?);
        // Allocate and construct the volume.
        let mem_disk = Arc::new(crucible::InMemoryBlockIO::new(
            Uuid::new_v4(),
            block_size,
            size as usize,
        ));
        let mut builder = VolumeBuilder::new(block_size, log);
        builder
            .add_subvolume(mem_disk)
            .await
            .map_err(|e| std::io::Error::from(e))?;

        let info = block::DeviceInfo {
            block_size: block_size as u32,
            total_size: size / block_size,
            read_only: opts.read_only.unwrap_or(false),
            supports_discard: false,
        };

        Ok(Arc::new(CrucibleBackend {
            block_attach: block::BackendAttachment::new(WORKER_COUNT, info),
            state: Arc::new(WorkerState {
                volume: builder.into(),
                info,
                skip_flush: opts.skip_flush.unwrap_or(false),
            }),
            workers: TaskGroup::new(),
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
    pub async fn get_uuid(&self) -> io::Result<uuid::Uuid> {
        self.state.volume.get_uuid().await.map_err(CrucibleError::into)
    }

    /// Issue a snapshot request
    pub async fn snapshot(&self, snapshot_id: Uuid) -> io::Result<()> {
        self.state
            .volume
            .flush(Some(SnapshotDetails {
                snapshot_name: snapshot_id.to_string(),
            }))
            .await
            .map_err(CrucibleError::into)
    }

    /// Issue a VolumeConstructionRequest replacement
    pub async fn vcr_replace(
        &self,
        old_vcr_json: &str,
        new_vcr_json: &str,
    ) -> io::Result<ReplaceResult> {
        let old_vcr = serde_json::from_str(old_vcr_json)?;
        let new_vcr = serde_json::from_str(new_vcr_json)?;
        self.state
            .volume
            .target_replace(old_vcr, new_vcr)
            .await
            .map_err(CrucibleError::into)
    }

    fn spawn_workers(&self) {
        let max_workers = self.block_attach.max_workers().get();
        self.workers.extend((0..max_workers).map(|n| {
            let worker_state = self.state.clone();
            let wctx = self.block_attach.worker(n);

            tokio::spawn(async move {
                let Some(wctx) = wctx.activate_async() else {
                    return;
                };
                worker_state.process_loop(wctx).await
            })
        }))
    }

    pub async fn volume_is_active(&self) -> Result<bool, CrucibleError> {
        self.state.volume.query_is_active().await
    }
}

#[async_trait::async_trait]
impl block::Backend for CrucibleBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.block_attach
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.state.volume.activate().await?;
        self.block_attach.start();
        self.spawn_workers();
        Ok(())
    }
    async fn stop(&self) -> () {
        self.block_attach.stop();
        self.workers.join_all().await;
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid guest memory region")]
    BadGuestRegion,
    #[error("backend is read-only")]
    ReadOnly,
    #[error("operation not supported")]
    Unsupported,

    #[error("offset or length not multiple of blocksize")]
    BlocksizeMismatch,

    #[error("copied length {0} did not match expectation {1}")]
    CopyError(usize, usize),

    #[error("IO Error")]
    Io(#[from] io::Error),

    #[error("Crucible Error: {0}")]
    Crucible(#[from] CrucibleError),
}
impl From<Error> for block::Result {
    fn from(value: Error) -> Self {
        match value {
            Error::ReadOnly => block::Result::ReadOnly,
            Error::Unsupported => block::Result::Unsupported,
            _ => block::Result::Failure,
        }
    }
}

/// Calculate offset (in crucible::Block form) and length in blocksize
fn block_offset_count(
    off_bytes: usize,
    len_bytes: usize,
    block_size: usize,
) -> Result<(crucible::BlockIndex, usize), Error> {
    if off_bytes % block_size == 0 && len_bytes % block_size == 0 {
        Ok((
            crucible::BlockIndex((off_bytes / block_size) as u64),
            len_bytes / block_size,
        ))
    } else {
        Err(Error::BlocksizeMismatch)
    }
}

#[cfg(test)]
mod test {
    use super::block_offset_count;

    #[test]
    fn err_on_bad_offset() {
        let bs = 512;
        assert!(block_offset_count(bs - 1, bs * 2, bs).is_err());
        assert!(block_offset_count(bs + 1, bs * 2, bs).is_err());
    }

    #[test]
    fn err_on_bad_size() {
        let bs = 512;
        assert!(block_offset_count(0, bs + 1, bs).is_err());
        assert!(block_offset_count(0, bs - 1, bs).is_err());
    }

    #[test]
    fn ok_for_valid() {
        let bs = 512;
        assert!(block_offset_count(0, bs, bs).is_ok());
        assert!(block_offset_count(bs * 3, bs * 4, bs).is_ok());
    }

    #[test]
    fn block_calc_ok() {
        let bs = 512;
        let off = bs * 4;
        let (block, _len) = block_offset_count(off, 0, bs).unwrap();

        assert_eq!(block.0, 4);
    }
}
