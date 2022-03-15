use std::io::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use super::DeviceInfo;
use crate::block;
use crate::dispatch::{DispCtx, Dispatcher};
use crate::inventory::Entity;
use crate::vmm::SubMapping;

pub struct InMemoryBackend {
    bytes: Arc<Mutex<Vec<u8>>>,

    driver: Mutex<Option<Arc<block::Driver>>>,

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

        let this = Self {
            bytes: Arc::new(Mutex::new(bytes)),

            driver: Mutex::new(None),

            read_only,
            block_size,
            sectors: len / block_size,
        };

        Ok(Arc::new(this))
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

    fn attach(
        &self,
        dev: Arc<dyn block::Device>,
        disp: &Dispatcher,
    ) -> Result<()> {
        let mut driverg = self.driver.lock().unwrap();
        assert!(driverg.is_none());

        let bytes = Arc::clone(&self.bytes);
        let read_only = self.read_only;
        let req_handler =
            Box::new(move |req: &block::Request, ctx: &DispCtx| {
                process_request(&bytes, req, ctx, read_only)
            });

        // Spawn driver to service block dev requests
        let driver = Arc::new(block::Driver::new(dev, req_handler));
        driver.spawn("mem", NonZeroUsize::new(1).unwrap(), disp)?;
        *driverg = Some(driver);

        Ok(())
    }
}

impl Entity for InMemoryBackend {
    fn type_name(&self) -> &'static str {
        "in-memory"
    }
}

/// Read from bytes into guest memory
fn process_read_request(
    bytes: &Mutex<Vec<u8>>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> Result<()> {
    let bytes = bytes.lock().unwrap();

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
    bytes: &Mutex<Vec<u8>>,
    offset: u64,
    len: usize,
    mappings: &Vec<SubMapping>,
) -> Result<()> {
    let mut bytes = bytes.lock().unwrap();

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

fn process_request(
    bytes: &Mutex<Vec<u8>>,
    req: &block::Request,
    ctx: &DispCtx,
    read_only: bool,
) -> Result<()> {
    let mem = ctx.mctx.memctx();
    match req.oper() {
        block::Operation::Read(off) => {
            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_read_request(bytes, off as u64, req.len(), &maps)?;
        }
        block::Operation::Write(off) => {
            if read_only {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    "backend is read-only",
                ));
            }

            let maps = req.mappings(&mem).ok_or_else(|| {
                Error::new(ErrorKind::Other, "bad guest region")
            })?;

            process_write_request(bytes, off as u64, req.len(), &maps)?;
        }
        block::Operation::Flush(_off, _len) => {
            // nothing to do
        }
    }

    Ok(())
}
