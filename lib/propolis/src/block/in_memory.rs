// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::accessors::MemAccessor;
use crate::block;
use crate::migrate::{
    MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
    PayloadOutput,
};
use crate::tasks::ThreadGroup;
use crate::vmm::{MemCtx, SubMapping};

use anyhow::Context;

use super::Lifecycle;

pub struct InMemoryBackend {
    state: Arc<WorkingState>,

    worker_count: NonZeroUsize,
    workers: ThreadGroup,
}
struct WorkingState {
    attachment: block::BackendAttachment,
    bytes: Mutex<Vec<u8>>,
    info: block::DeviceInfo,
}
impl WorkingState {
    fn processing_loop(&self, acc_mem: MemAccessor) {
        while let Some(req) = self.attachment.block_for_req() {
            if self.info.read_only && req.oper().is_write() {
                req.complete(block::Result::ReadOnly);
                continue;
            }

            let mem = match acc_mem.access() {
                Some(m) => m,
                None => {
                    req.complete(block::Result::Failure);
                    continue;
                }
            };
            let res = match self.process_request(&req, &mem) {
                Ok(_) => block::Result::Success,
                Err(_) => block::Result::Failure,
            };
            req.complete(res);
        }
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> Result<()> {
        match req.oper() {
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

        Ok(Arc::new(Self {
            state: Arc::new(WorkingState {
                attachment: block::BackendAttachment::new(),
                bytes: Mutex::new(bytes),
                info: block::DeviceInfo {
                    block_size,
                    total_size: len as u64 / u64::from(block_size),
                    read_only: opts.read_only.unwrap_or(false),
                },
            }),
            worker_count,
            workers: ThreadGroup::new(),
        }))
    }
    fn spawn_workers(&self) -> Result<()> {
        let spawn_results = (0..self.worker_count.get()).map(|n| {
            let worker_state = self.state.clone();
            let worker_acc = self
                .state
                .attachment
                .accessor_mem(|mem| mem.child(Some(format!("worker {n}"))))
                .expect("backend is attached");

            std::thread::Builder::new()
                .name(format!("in-memory worker {n}"))
                .spawn(move || {
                    worker_state.processing_loop(worker_acc);
                })
        });

        self.workers.extend(spawn_results.into_iter())
    }
}

#[async_trait::async_trait]
impl block::Backend for InMemoryBackend {
    fn attachment(&self) -> &block::BackendAttachment {
        &self.state.attachment
    }

    fn info(&self) -> block::DeviceInfo {
        self.state.info
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.state.attachment.start();
        if let Err(e) = self.spawn_workers() {
            self.state.attachment.stop();
            self.workers.block_until_joined();
            Err(e).context("failure while spawning workers")
        } else {
            Ok(())
        }
    }

    async fn stop(&self) -> () {
        self.state.attachment.stop();
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

    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}

impl MigrateSingle for InMemoryBackend {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<PayloadOutput, MigrateStateError> {
        let bytes = self.state.bytes.lock().unwrap();
        Ok(migrate::InMemoryBlockBackendV1 { bytes: bytes.clone() }.into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> std::result::Result<(), MigrateStateError> {
        let data: migrate::InMemoryBlockBackendV1 = offer.parse()?;
        let mut guard = self.state.bytes.lock().unwrap();
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
