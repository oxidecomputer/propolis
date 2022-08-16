//! Implement a virtual block device backed by Crucible

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Weak};

use super::DeviceInfo;
use crate::block;
use crate::inventory::Entity;
use crate::vmm::{MemCtx, SubMapping};

use crucible::{
    crucible_bail, BlockIO, Buffer, CrucibleError, SnapshotDetails, Volume,
};
use crucible_client_types::VolumeConstructionRequest;
use oximeter::types::ProducerRegistry;
use uuid::Uuid;

/// Helper function, because Rust couldn't derive the types
fn map_crucible_error_to_io(x: CrucibleError) -> std::io::Error {
    x.into()
}

pub struct CrucibleBackend {
    block_io: Arc<dyn BlockIO + Send + Sync>,
    driver: block::Driver,
    log: slog::Logger,

    read_only: bool,
    block_size: u64,
    sectors: u64,
}

impl CrucibleBackend {
    pub fn create(
        gen: u64,
        request: VolumeConstructionRequest,
        read_only: bool,
        producer_registry: Option<ProducerRegistry>,
        log: slog::Logger,
    ) -> Result<Arc<Self>> {
        CrucibleBackend::_create(
            gen,
            request,
            read_only,
            producer_registry,
            log,
        )
        .map_err(map_crucible_error_to_io)
    }

    fn _create(
        gen: u64,
        request: VolumeConstructionRequest,
        read_only: bool,
        producer_registry: Option<ProducerRegistry>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>, crucible::CrucibleError> {
        // XXX Crucible uses std::sync::mpsc::Receiver, not
        // tokio::sync::mpsc::Receiver, so use tokio::task::block_in_place here.
        // Remove that when Crucible changes over to the tokio mpsc.
        let volume = Arc::new(tokio::task::block_in_place(|| {
            Volume::construct(request, producer_registry)
        })?);

        volume.activate(gen)?;

        // After active negotiation, set sizes
        let block_size =
            tokio::task::block_in_place(|| volume.get_block_size())?;
        let total_size = tokio::task::block_in_place(|| volume.total_size())?;
        let sectors = total_size / block_size;

        // TODO: make this tunable?
        let worker_count = NonZeroUsize::new(8).unwrap();

        Ok(Arc::new_cyclic(|me| Self {
            block_io: volume,
            driver: block::Driver::new(
                me.clone() as Weak<dyn block::Backend>,
                "crucible-bdev".to_string(),
                worker_count,
            ),
            log,

            read_only,
            block_size,
            sectors,
        }))
    }

    /// Retrieve the UUID identifying this Crucible backend.
    pub fn get_uuid(&self) -> Result<uuid::Uuid> {
        self.block_io.get_uuid().map_err(map_crucible_error_to_io)
    }

    /// Issue a snapshot request
    pub fn snapshot(&self, snapshot_id: Uuid) -> Result<()> {
        // XXX Crucible uses std::sync::mpsc::Receiver, not
        // tokio::sync::mpsc::Receiver, so use tokio::task::block_in_place here.
        // Remove that when Crucible changes over to the tokio mpsc.
        let mut waiter = tokio::task::block_in_place(|| {
            self.block_io.flush(Some(SnapshotDetails {
                snapshot_name: snapshot_id.to_string(),
            }))
        })
        .map_err(map_crucible_error_to_io)?;

        tokio::task::block_in_place(|| waiter.block_wait())
            .map_err(map_crucible_error_to_io)?;

        Ok(())
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> Result<()> {
        let block_io = &*self.block_io;
        let read_only = self.read_only;

        match req.oper() {
            block::Operation::Read(off) => {
                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                process_read_request(block_io, off as u64, req.len(), &maps)
                    .map_err(map_crucible_error_to_io)?;
            }
            block::Operation::Write(off) => {
                if read_only {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "backend is read-only",
                    ));
                }

                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                process_write_request(block_io, off as u64, req.len(), &maps)
                    .map_err(map_crucible_error_to_io)?;
            }
            block::Operation::Flush(_off, _len) => {
                process_flush_request(block_io)
                    .map_err(map_crucible_error_to_io)?;
            }
        }

        Ok(())
    }
}

impl block::Backend for CrucibleBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors as u64,
            writable: !self.read_only,
        }
    }

    fn process(&self, req: &block::Request, mem: &MemCtx) -> block::Result {
        match self.process_request(req, mem) {
            Ok(_) => block::Result::Success,
            Err(e) => {
                slog::info!(self.log, "block IO error {:?}", req.op; "error" => e);
                block::Result::Failure
            }
        }
    }

    fn attach(&self, dev: Arc<dyn block::Device>) -> Result<()> {
        self.driver.attach(dev)
    }
}

impl Entity for CrucibleBackend {
    fn type_name(&self) -> &'static str {
        "block-crucible"
    }
    fn run(&self) {
        self.driver.run();
    }
}

/// Perform one large read from crucible, and write from data into mappings
fn process_read_request(
    block_io: &(dyn BlockIO + Send + Sync),
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let data = Buffer::new(len);
    let offset = block_io.byte_offset_to_block(offset)?;

    let mut waiter = block_io.read(offset, data.clone())?;
    waiter.block_wait()?;

    let mut nwritten = 0;
    for mapping in mappings {
        nwritten += mapping.write_bytes(
            &data.as_vec()[nwritten..(nwritten + mapping.len())],
        )?;
    }

    if nwritten as usize != len {
        crucible_bail!(IoError, "nwritten != len! {} vs {}", nwritten, len);
    }

    Ok(())
}

/// Read from all the mappings into vec, and perform one large write to crucible
fn process_write_request(
    block_io: &(dyn BlockIO + Send + Sync),
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> std::result::Result<(), CrucibleError> {
    let mut vec: Vec<u8> = vec![0; len];

    let mut nread = 0;
    for mapping in mappings {
        nread +=
            mapping.read_bytes(&mut vec[nread..(nread + mapping.len())])?;
    }

    let offset = block_io.byte_offset_to_block(offset)?;

    let mut waiter = block_io.write(offset, crucible::Bytes::from(vec))?;
    waiter.block_wait()?;

    if nread as usize != len {
        crucible_bail!(IoError, "nread != len! {} vs {}", nread, len);
    }

    Ok(())
}

/// Send flush to crucible
fn process_flush_request(
    block_io: &(dyn BlockIO + Send + Sync),
) -> std::result::Result<(), CrucibleError> {
    let mut waiter = block_io.flush(None)?;
    waiter.block_wait()?;

    Ok(())
}
