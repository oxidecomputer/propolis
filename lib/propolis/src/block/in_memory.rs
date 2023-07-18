// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use super::DeviceInfo;
use crate::block;
use crate::inventory::Entity;
use crate::vmm::{MemCtx, SubMapping};

pub struct InMemoryBackend {
    bytes: Mutex<Vec<u8>>,
    driver: block::Driver,

    read_only: bool,
    block_size: usize,
    sectors: usize,
}

impl InMemoryBackend {
    pub fn create(
        bytes: Vec<u8>,
        read_only: bool,
        block_size: usize,
    ) -> Result<Arc<Self>> {
        match block_size {
            512 | 4096 => {
                // ok
            }
            _ => {
                return Err(std::io::Error::new(
                    ErrorKind::Other,
                    format!("unsupported block size {}!", block_size,),
                ));
            }
        }

        let len = bytes.len();

        if (len % block_size) != 0 {
            return Err(std::io::Error::new(
                ErrorKind::Other,
                format!(
                    "in-memory bytes length {} not multiple of block size {}!",
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

            read_only,
            block_size,
            sectors: len / block_size,
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
                if self.read_only {
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
            block::Operation::Flush(_off, _len) => {
                // nothing to do
            }
        }

        Ok(())
    }
}

impl block::Backend for InMemoryBackend {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            block_size: self.block_size as u32,
            total_size: self.sectors as u64,
            writable: !self.read_only,
        }
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
