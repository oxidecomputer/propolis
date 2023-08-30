// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use crate::block;
use crate::inventory::Entity;
use crate::vmm::{MemCtx, SubMapping};

pub struct InMemoryBackend {
    bytes: Mutex<Vec<u8>>,
    driver: block::Driver,

    info: block::DeviceInfo,
}

impl InMemoryBackend {
    pub fn create(
        bytes: Vec<u8>,
        opts: block::BackendOpts,
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

        Ok(Arc::new_cyclic(|me| Self {
            bytes: Mutex::new(bytes),

            driver: block::Driver::new(
                me.clone() as Weak<dyn block::Backend>,
                "mem-bdev".to_string(),
                NonZeroUsize::new(1).unwrap(),
            ),

            info: block::DeviceInfo {
                block_size,
                total_size: len as u64 / block_size as u64,
                read_only: opts.read_only.unwrap_or(false),
            },
        }))
    }

    fn process_request(
        &self,
        req: &block::Request,
        mem: &MemCtx,
    ) -> Result<()> {
        match req.oper() {
            block::Operation::Read(off) => {
                let maps = req.mappings(mem).ok_or_else(|| {
                    Error::new(ErrorKind::Other, "bad guest region")
                })?;

                let bytes = self.bytes.lock().unwrap();
                process_read_request(&bytes, off as u64, req.len(), &maps)?;
            }
            block::Operation::Write(off) => {
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
                process_write_request(
                    &mut bytes,
                    off as u64,
                    req.len(),
                    &maps,
                )?;
            }
            block::Operation::Flush => {
                // nothing to do
            }
        }

        Ok(())
    }
}

impl block::Backend for InMemoryBackend {
    fn info(&self) -> block::DeviceInfo {
        self.info
    }

    fn attach(&self, dev: Arc<dyn block::Device>) -> Result<()> {
        self.driver.attach(dev)
    }

    fn process(&self, req: &block::Request, mem: &MemCtx) -> block::Result {
        match self.process_request(req, mem) {
            Ok(_) => block::Result::Success,
            Err(_e) => {
                // TODO: add detail
                //slog::info!(self.log, "block IO error {:?}", req.op; "error" => e);
                block::Result::Failure
            }
        }
    }
}

impl Entity for InMemoryBackend {
    fn type_name(&self) -> &'static str {
        "block-in-memory"
    }
    fn start(&self) -> anyhow::Result<()> {
        self.driver.start();
        Ok(())
    }
    fn pause(&self) {
        self.driver.pause();
    }
    fn resume(&self) {
        self.driver.resume();
    }
    fn halt(&self) {
        self.driver.halt();
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
