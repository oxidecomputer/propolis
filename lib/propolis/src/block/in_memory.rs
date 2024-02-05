// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::accessors::MemAccessor;
use crate::block;
use crate::vmm::{MemCtx, SubMapping};

pub struct InMemoryBackend {
    state: Arc<WorkingState>,

    worker_count: NonZeroUsize,
}
struct WorkingState {
    attachment: block::backend::Attachment,
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
                attachment: block::backend::Attachment::new(),
                bytes: Mutex::new(bytes),
                info: block::DeviceInfo {
                    block_size,
                    total_size: len as u64 / block_size as u64,
                    read_only: opts.read_only.unwrap_or(false),
                },
            }),
            worker_count,
        }))
    }
    fn spawn_workers(&self) -> Result<()> {
        for n in 0..self.worker_count.get() {
            let worker_state = self.state.clone();
            let worker_acc = self.state.attachment.accessor_mem(|mem| {
                mem.expect("backend is attached")
                    .child(Some(format!("worker {n}")))
            });

            let _join = std::thread::Builder::new()
                .name(format!("in-memory worker {n}"))
                .spawn(move || {
                    worker_state.processing_loop(worker_acc);
                })?;
        }
        Ok(())
    }
}

impl block::Backend for InMemoryBackend {
    fn attachment(&self) -> &block::backend::Attachment {
        &self.state.attachment
    }
    fn info(&self) -> block::DeviceInfo {
        self.state.info
    }
    fn start(&self) -> anyhow::Result<()> {
        self.state.attachment.start();
        self.spawn_workers()?;
        Ok(())
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
