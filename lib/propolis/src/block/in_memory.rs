// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::block;
use crate::common::Lifecycle;
use crate::migrate::{
    MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
    PayloadOutput,
};
use crate::tasks::ThreadGroup;
use crate::vmm::{MemCtx, SubMapping};

use anyhow::Context;

pub struct InMemoryBackend {
    shared_state: Arc<SharedState>,
    block_attach: block::BackendAttachment,

    workers: ThreadGroup,
}
struct SharedState {
    bytes: Mutex<Vec<u8>>,
    info: block::DeviceInfo,
}
impl SharedState {
    fn processing_loop(&self, wctx: block::SyncWorkerCtx) {
        while let Some(dreq) = wctx.block_for_req() {
            let req = dreq.req();
            if self.info.read_only && req.op.is_write() {
                dreq.complete(block::Result::ReadOnly);
                continue;
            }
            if req.op.is_discard() {
                // Punt on discard support
                dreq.complete(block::Result::Unsupported);
                continue;
            }

            let res = match wctx
                .acc_mem()
                .access()
                .and_then(|mem| self.process_request(&req, &mem).ok())
            {
                Some(_) => block::Result::Success,
                None => block::Result::Failure,
            };

            dreq.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> Result<()> {
        match req.op {
            block::Operation::Read(off, len) => {
                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                let bytes = self.bytes.lock().unwrap();
                process_read_request(&bytes, off as u64, len, &maps)?;
            }
            block::Operation::Write(off, len) => {
                if self.info.read_only {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "backend is read-only",
                    ));
                }

                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                let mut bytes = self.bytes.lock().unwrap();
                process_write_request(&mut bytes, off as u64, len, &maps)?;
            }
            block::Operation::Flush => {
                // nothing to do
            }
            block::Operation::Discard(..) => {
                unreachable!("handled in processing_loop()");
            }
        }

        Ok(())
    }
}

impl InMemoryBackend {
    pub fn create(
        bytes: Vec<u8>,
        opts: block::BackendOpts,
        worker_count: NonZeroUsize,
    ) -> Result<Arc<Self>> {
        let block_size = opts.block_size.unwrap_or(block::DEFAULT_BLOCK_SIZE);

        let len = bytes.len();
        if len == 0 {
            return Err(Error::new(ErrorKind::Other, "size cannot be 0"));
        } else if (len % block_size as usize) != 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "size {} not multiple of block size {}!",
                    len, block_size,
                ),
            ));
        }

        let info = block::DeviceInfo {
            block_size,
            total_size: len as u64 / u64::from(block_size),
            read_only: opts.read_only.unwrap_or(false),
            supports_discard: false,
        };
        let bytes = Mutex::new(bytes);
        let block_attach = block::BackendAttachment::new(worker_count, info);

        Ok(Arc::new(Self {
            shared_state: Arc::new(SharedState { bytes, info }),
            block_attach,

            workers: ThreadGroup::new(),
        }))
    }
    fn spawn_workers(&self) -> Result<()> {
        let count = self.block_attach.max_workers().get();
        let spawn_results = (0..count).map(|n| {
            let shared_state = self.shared_state.clone();
            let wctx = self.block_attach.worker(n);
            std::thread::Builder::new()
                .name(format!("in-memory worker {n}"))
                .spawn(move || {
                    let wctx = wctx
                        .activate_sync()
                        .expect("worker slot is uncontended");
                    shared_state.processing_loop(wctx);
                })
        });

        self.workers.extend(spawn_results.into_iter())
    }
}

#[async_trait::async_trait]
impl block::Backend for InMemoryBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.block_attach
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.block_attach.start();
        if let Err(e) = self.spawn_workers() {
            self.block_attach.stop();
            self.workers.block_until_joined();
            Err(e).context("failure while spawning workers")
        } else {
            Ok(())
        }
    }

    async fn stop(&self) -> () {
        self.block_attach.stop();
        self.workers.block_until_joined();
    }
}

/// Read from bytes into guest memory
fn process_read_request(
    bytes: &[u8],
    offset: u64,
    len: usize,
    mappings: &[SubMapping],
) -> Result<()> {
    let start = offset as usize;
    let end = offset as usize + len;

    if start >= bytes.len() || end > bytes.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid offset {} and len {} when bytes len is {}",
                offset,
                len,
                bytes.len(),
            ),
        ));
    }

    let data = &bytes[start..end];

    let mut nwritten = 0;
    for mapping in mappings {
        nwritten +=
            mapping.write_bytes(&data[nwritten..(nwritten + mapping.len())])?;
    }

    Ok(())
}

/// Write from guest memory into bytes
fn process_write_request(
    bytes: &mut [u8],
    offset: u64,
    len: usize,
    mappings: &[SubMapping],
) -> Result<()> {
    let start = offset as usize;
    let end = offset as usize + len;

    if start >= bytes.len() || end > bytes.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid offset {} and len {} when bytes len is {}",
                offset,
                len,
                bytes.len(),
            ),
        ));
    }

    let data = &mut bytes[start..end];

    let mut nread = 0;
    for mapping in mappings {
        nread +=
            mapping.read_bytes(&mut data[nread..(nread + mapping.len())])?;
    }

    Ok(())
}

impl Lifecycle for InMemoryBackend {
    fn type_name(&self) -> &'static str {
        "in-memory-storage"
    }

    fn migrate(&self) -> Migrator<'_> {
        Migrator::Single(self)
    }
}

impl MigrateSingle for InMemoryBackend {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<PayloadOutput, MigrateStateError> {
        let bytes = self.shared_state.bytes.lock().unwrap();
        Ok(migrate::InMemoryBlockBackendV1 { bytes: bytes.clone() }.into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<(), MigrateStateError> {
        let data: migrate::InMemoryBlockBackendV1 = offer.parse()?;
        let mut guard = self.shared_state.bytes.lock().unwrap();
        if guard.len() != data.bytes.len() {
            return Err(MigrateStateError::ImportFailed(format!(
                "imported in-memory block backend data has length {}, \
                        but backend's original length was {}",
                data.bytes.len(),
                guard.len()
            )));
        }

        *guard = data.bytes;
        Ok(())
    }
}

mod migrate {
    use serde::{Deserialize, Serialize};

    use crate::migrate::{Schema, SchemaId};

    #[derive(Serialize, Deserialize)]
    pub struct InMemoryBlockBackendV1 {
        pub(super) bytes: Vec<u8>,
    }

    impl std::fmt::Debug for InMemoryBlockBackendV1 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("InMemoryBlockBackendV1")
                .field("bytes", &"<redacted>".to_string())
                .finish()
        }
    }

    impl Schema<'_> for InMemoryBlockBackendV1 {
        fn id() -> SchemaId {
            ("in-memory-block-backend", 1)
        }
    }
}
