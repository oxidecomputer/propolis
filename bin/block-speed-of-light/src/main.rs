// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use rand::Rng;
use serde::{Deserialize, Serialize};
use slog::info;
use slog::o;
use slog::Drain;
use slog::Logger;

use propolis::accessors;
use propolis::block;
use propolis::common;
use propolis::vmm;

// XXX common all this with standalone

const DEFAULT_WORKER_COUNT: usize = 8;
const MAX_FILE_WORKERS: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub main: Main,

    #[serde(default, rename = "block_dev")]
    pub block_devs: BTreeMap<String, BlockDevice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Main {
    pub ram_path: String,
    pub max_queues: NonZeroUsize,
    pub io_depth: NonZeroUsize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockOpts {
    pub block_size: Option<u32>,
    pub read_only: Option<bool>,
    pub skip_flush: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockDevice {
    #[serde(default, rename = "type")]
    pub bdtype: String,

    #[serde(flatten)]
    pub block_opts: BlockOpts,

    #[serde(flatten, default)]
    pub options: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
struct FileConfig {
    path: String,
    workers: Option<NonZeroUsize>,
}

#[derive(Deserialize)]
struct MemAsyncConfig {
    size: u64,
    workers: Option<usize>,
}

fn get_backends(log: &Logger, config: &Config) -> Vec<Arc<dyn block::Backend>> {
    let mut backends = Vec::with_capacity(config.block_devs.len());

    for (name, block_device) in &config.block_devs {
        let be = block_backend(log, name, block_device);
        backends.push(be);
    }

    backends
}

// Try to turn unmatched flattened options into a config struct
fn opt_deser<'de, T: Deserialize<'de>>(
    value: &BTreeMap<String, toml::Value>,
) -> Result<T, anyhow::Error> {
    let map = toml::map::Map::from_iter(value.clone());
    let config = map.try_into::<T>()?;
    Ok(config)
}

pub fn block_backend(
    log: &Logger,
    backend_name: &str,
    block_device: &BlockDevice,
) -> Arc<dyn block::Backend> {
    let opts = block::BackendOpts {
        block_size: block_device.block_opts.block_size,
        read_only: block_device.block_opts.read_only,
        skip_flush: block_device.block_opts.skip_flush,
    };

    match &block_device.bdtype as &str {
        "file" => {
            let parsed: FileConfig = opt_deser(&block_device.options).unwrap();

            // Check if raw device is being used and gripe if it isn't
            let meta = std::fs::metadata(&parsed.path)
                .with_context(|| {
                    format!(
                        "opening {} for block device \"{backend_name}\"",
                        parsed.path,
                    )
                })
                .expect("file device path is valid");

            if meta.file_type().is_block_device() {
                slog::warn!(log, "Block backend using standard device rather than raw";
                    "path" => &parsed.path);
            }

            let workers: NonZeroUsize = match parsed.workers {
                Some(workers) => {
                    if workers.get() <= MAX_FILE_WORKERS {
                        workers
                    } else {
                        slog::warn!(
                            log,
                            "workers must be between 1 and {} \
                            Using default value of {}.",
                            MAX_FILE_WORKERS,
                            DEFAULT_WORKER_COUNT,
                        );
                        NonZeroUsize::new(DEFAULT_WORKER_COUNT).unwrap()
                    }
                }
                None => NonZeroUsize::new(DEFAULT_WORKER_COUNT).unwrap(),
            };
            block::FileBackend::create(&parsed.path, opts, workers).unwrap()
        }

        /*
        "crucible" => create_crucible_backend(be, opts, log),

        "crucible-mem" => create_crucible_mem_backend(be, opts, log),
        */
        "mem-async" => {
            let parsed: MemAsyncConfig =
                opt_deser(&block_device.options).unwrap();

            block::MemAsyncBackend::create(
                parsed.size,
                opts,
                NonZeroUsize::new(
                    parsed.workers.unwrap_or(DEFAULT_WORKER_COUNT),
                )
                .unwrap(),
            )
            .unwrap()
        }

        _ => {
            panic!("unrecognized block dev type {}!", block_device.bdtype);
        }
    }
}

fn build_log(level: slog::Level) -> slog::Logger {
    let main_drain = if atty::is(atty::Stream::Stdout) {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    } else {
        let drain =
            slog_bunyan::with_name("propolis-standalone", std::io::stdout())
                .build()
                .fuse();
        slog_async::Async::new(drain)
            .overflow_strategy(slog_async::OverflowStrategy::Block)
            .build_no_guard()
    };

    let (dtrace_drain, probe_reg) = slog_dtrace::Dtrace::new();

    let filtered_main = slog::LevelFilter::new(main_drain, level);

    let log = slog::Logger::root(
        slog::Duplicate::new(filtered_main.fuse(), dtrace_drain.fuse()).fuse(),
        o!(),
    );

    if let slog_dtrace::ProbeRegistration::Failed(err) = probe_reg {
        slog::error!(&log, "Error registering slog-dtrace probes: {:?}", err);
    }

    log
}

pub fn config_parse(path: &str) -> anyhow::Result<Config> {
    let file_data =
        std::fs::read(path).context("Failed to read given config.toml")?;

    Ok(toml::from_str::<Config>(
        std::str::from_utf8(&file_data)
            .context("config should be valid utf-8")?,
    )?)
}

#[derive(clap::Parser)]
struct Args {
    #[clap(value_name = "config", action)]
    config: String,
}

struct AttachedBackend {
    pub block_attach: block::DeviceAttachment,
    pub backend: Arc<dyn block::Backend>,
}

struct Timer {
    log: Logger,
    count: usize,
    time: Instant,
}

impl Timer {
    fn tick(&mut self) {
        self.count += 1;

        let now = Instant::now();

        if (now - self.time) >= std::time::Duration::from_secs(5) {
            info!(self.log, "{} requests/second", self.count / 5);
            self.count = 0;
            self.time = now;
        }
    }
}

struct RandWriteSpew {
    #[allow(unused)]
    log: Logger,

    #[allow(unused)]
    info: block::DeviceInfo,

    acc_mem: accessors::MemAccessor,

    timer: Arc<Mutex<Timer>>,

    // into memory mapping
    offset: usize,

    // TODO io size
}

impl block::DeviceQueue for RandWriteSpew {
    type Token = ();

    fn next_req(
        &self,
    ) -> Option<(block::Request, Self::Token, Option<Instant>)> {
        let guest_addr: u64 = self.offset as u64;

        {
            let mut data = vec![1u8; 4096];
            rand::rng().fill(&mut data[..]);

            let mem = self.acc_mem.access().unwrap();
            mem.write_from(common::GuestAddr(guest_addr), &data, 4096);
        }

        let guest_region =
            common::GuestRegion(common::GuestAddr(guest_addr), 4096);

        let write_offset: usize =
            rand::rng().random_range(0..(self.info.total_size_in_bytes() as usize - 4096));

        let request =
            block::Request::new_write(write_offset, 4096, vec![guest_region]);

        Some((request, (), None))
    }

    fn complete(
        &self,
        _op: block::Operation,
        result: block::Result,
        _token: Self::Token,
    ) {
        match result {
            block::Result::Success => {}
            _ => {
                panic!("not success!");
            }
        }

        let mut timer = self.timer.lock().unwrap();
        timer.tick();
    }
}

struct FileBackedPhysMap {
    size: usize,
    fp: File,
    segments: Mutex<BTreeMap<i32, (usize, Option<String>)>>,
}

impl FileBackedPhysMap {
    pub fn new(fp: File) -> Self {
        let size = fp.metadata().unwrap().len() as usize;

        FileBackedPhysMap {
            size,
            fp,
            segments: Mutex::new(BTreeMap::default()),
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

impl vmm::PhysMapHdl for FileBackedPhysMap {
    fn fd(&self) -> RawFd {
        self.fp.as_raw_fd()
    }

    fn create_memseg(
        &self,
        segid: i32,
        size: usize,
        segname: Option<&str>,
    ) -> std::io::Result<()> {
        let mut segments = self.segments.lock().unwrap();
        let old =
            segments.insert(segid, (size, segname.map(|x| x.to_string())));
        assert!(old.is_none());
        Ok(())
    }

    fn map_memseg(
        &self,
        segid: i32,
        _gpa: usize,
        _len: usize,
        _segoff: usize,
        _prot: vmm::Prot,
    ) -> std::io::Result<()> {
        let segments = self.segments.lock().unwrap();
        assert!(segments.contains_key(&segid));
        Ok(())
    }

    fn devmem_offset(&self, segid: i32) -> std::io::Result<usize> {
        let segments = self.segments.lock().unwrap();
        let mut offset = 0;
        for (id, (size, _)) in segments.iter() {
            if *id == segid {
                return Ok(offset);
            }
            offset += size;
        }
        panic!("segment not found");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let Args { config } = Args::parse();

    let log = build_log(slog::Level::Info);

    let config = config_parse(&config)?;

    let backends = get_backends(&log, &config);

    let file_backed_ram = Arc::new(FileBackedPhysMap::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(&config.main.ram_path)?,
    ));

    let mut phys_map = vmm::PhysMap::new(
        usize::MAX - (usize::MAX / common::PAGE_SIZE),
        file_backed_ram.clone(),
    );

    phys_map.add_mem(String::from("ram"), 0, file_backed_ram.size())?;

    let root_acc_mem = phys_map.finalize();

    let mut attached_backends = Vec::with_capacity(backends.len());

    for backend in backends {
        let acc_mem = root_acc_mem.child(Some("block backend".to_string()));

        let block_attach =
            block::DeviceAttachment::new(config.main.max_queues, acc_mem);

        block::attach(&block_attach, backend.attachment()).unwrap();

        backend.start().await?;

        attached_backends.push(AttachedBackend { block_attach, backend });
    }

    let mut jobs = Vec::with_capacity(attached_backends.len());

    for (n, attached_backend) in attached_backends.into_iter().enumerate() {
        let log = log.new(o!("backend" => n));
        info!(log, "starting");

        let acc_mem = root_acc_mem.child(Some("rand write spew".to_string()));

        let job = std::thread::Builder::new()
            .name(format!("attached backend {n}"))
            .spawn(move || {
                let timer = Arc::new(Mutex::new(Timer {
                    log: log.clone(),
                    count: 0,
                    time: Instant::now(),
                }));

                let max_queues: usize = config.main.max_queues.into();

                for m in 0..max_queues {
                    let acc_mem = acc_mem
                        .child(Some(format!("rand write spew queue {n}")));

                    let spew = RandWriteSpew {
                        log: log.new(o!("spew" => m)),
                        info: attached_backend.backend.device_info(),
                        timer: timer.clone(),
                        acc_mem,
                        offset: 4096 * m,
                    };

                    info!(log, "associating with spew {m}");

                    attached_backend
                        .block_attach
                        .queue_associate(m.into(), Arc::new(spew));
                }

                loop {
                    for m in 0..max_queues {
                        attached_backend
                            .block_attach
                            .notify(m.into(), Some(config.main.io_depth));
                    }
                }
            })?;

        jobs.push(job);
    }

    for job in jobs {
        job.join().unwrap();
    }

    Ok(())
}
