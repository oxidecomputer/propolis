// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Result, Write},
    num::NonZeroU16,
    sync::{Arc, Mutex},
    thread::{sleep, spawn},
    time::Duration,
};

use crate::{
    chardev::{Sink, Source},
    common::*,
    hw::{pci, uart::LpcUart},
    migrate::Migrator,
    util::regmap::RegMap,
    vmm::MemCtx,
};

use super::{
    bits::*,
    pci::{PciVirtio, PciVirtioState},
    queue::{write_buf, Chain, VirtQueue, VirtQueues},
    viona::bits::VIRTIO_NET_S_LINK_UP,
    VirtioDevice,
};

use softnpu_lib::mgmt::ManagementRequest;
use softnpu_lib::p4rs::{self, packet_in, packet_out, Pipeline};

use crate::hw::virtio::p9fs::{write_error, P9Handler, PciVirtio9pfs};
use dlpi::sys::dlpi_recvinfo_t;
use lazy_static::lazy_static;
use libc::ENOTSUP;
use libloading::os::unix::{Library, Symbol, RTLD_NOW};
use p9ds::proto::{Rclunk, Rwrite, Twrite};
use rand::Rng;
use serde::{Deserialize, Serialize};
use slog::{error, info, warn, Logger};

// Transit jumbo frames
const MTU: usize = 9216;
const SOFTNPU_CPU_AUX_PORT: u16 = 1000;

pub const MANAGEMENT_MESSAGE_PREAMBLE: u8 = 0b11100101;
pub const SOFTNPU_TTY: &str = "/dev/tty03";

/// A software network processing unit (SoftNpu) is an ASIC emulator. It's meant
/// to represent a P4 programmable ASIC such as those found in programmable
/// switches and NICs.
///
/// A SoftNpu instance can support a variable number of ports. These ports are
/// specified by the user as data link names through propolis configuration.
/// SoftNpu establishes a dlpi handle on each configured data link to perform
/// packet i/o.
///
/// When a SoftNpu device is instantiated there is no P4 program that runs by
/// default. A program must be loaded onto the emulated ASIC just like a real
/// ASIC. This is accomplished through the P9 file system device exposed by
/// SoftNpu. This P9 implementation exports a specific version string 9P2000.P4
/// and only implements file writes to allow a consumer to upload a P4 program.
///
/// SoftNpu takes pre-compiled P4 programs in the form of shared libraries.
/// These shared libraries must export a [pipeline constructor](
/// https://oxidecomputer.github.io/p4/p4rs/trait.Pipeline.html) under the
/// symbol `_main_pipeline_create`. Programs compiled with the `x4c` compiler
/// export this symbol automatically.
///
/// Once a pre-compiled P4 program is loaded, the Pipeline object from that
/// program is used to process packets. SoftNpu uses the illumos dlpi interface
/// to send and receive raw Ethernet frames from the data link devices it has
/// been configured with. Each frame received is processed with the loaded
/// pipeline. If the pipeline invocation returns an egress port, then the egress
/// packet returned by the pipeline will be sent out that port. If no egress
/// port is returned, the packet is dropped.
///
/// In addition to forwarding packets between ports, SoftNpu also supports
/// forwarding packets to and from the guest. This is accomplished through a
/// special `pci_port` device. This is a viona device that shows up in the guest
/// as a virtio network device. When a pipeline invocation returns an egress
/// port of `0`, packets are sent to this port.
///
/// Most P4 programs require a corresponding control plane program to manage
/// table state. For example, a program to add routing entries onto the ASIC. P4
/// programs themselves only handle packets, they are not capable of managing
/// table state. SoftNpu provides a uart-based management interface so that
/// programs running in the guest can modify the tables of the P4 program loaded
/// onto the ASIC. This is uart plumbed into the guest as `tty03`. What tables
/// exist and how they can be modified is up to the particular program that is
/// loaded. SoftNpu just provides a generic interface for table management and a
/// few other ASIC housekeeping items like determining the number of ports.
pub struct SoftNpu {
    /// Data links SoftNpu will hook into.
    pub data_links: Vec<String>,

    /// The PCI port.
    pub pci_port: Arc<PciVirtioSoftNpuPort>,

    /// UART for management from guest
    uart: Arc<LpcUart>,

    /// P9 file system endpoint for pre-compiled program transfer
    pub p9fs: Arc<PciVirtio9pfs>,

    booted: Mutex<bool>,

    /// Logging instance
    log: Logger,
}

unsafe impl Send for SoftNpu {}
unsafe impl Sync for SoftNpu {}

type LoadedP4Program = (Library, Box<dyn Pipeline>);

/// PciVirtioSoftNpuPort is a PCI device exposed to the guest as a virtio-net
/// device. This device represents a sidecar CPU port.
pub struct PciVirtioSoftNpuPort {
    /// Logging instance
    log: Logger,

    /// Virtio state to guest
    virtio_state: Arc<PortVirtioState>,

    /// dlpi handle for external i/o
    data_handles: Vec<dlpi::DlpiHandle>,

    mac: [u8; 6],

    //TODO should be able to do this as a RwLock
    pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
}

pub struct PortVirtioState {
    /// Underlying virtio state
    pci_virtio_state: PciVirtioState,

    /// Underlying PCI device state
    pci_state: pci::DeviceState,
}

impl PortVirtioState {
    fn new(queue_size: u16) -> Self {
        let queues = VirtQueues::new(
            NonZeroU16::new(queue_size).unwrap(),
            NonZeroU16::new(2).unwrap(), //TX and RX
        );
        let msix_count = Some(2);
        let (pci_virtio_state, pci_state) = PciVirtioState::create(
            queues,
            msix_count,
            VIRTIO_DEV_NET,
            VIRTIO_SUB_DEV_NET,
            pci::bits::CLASS_NETWORK,
            VIRTIO_NET_CFG_SIZE,
        );
        Self { pci_virtio_state, pci_state }
    }
}

impl SoftNpu {
    /// Create a new SoftNpu device for the specified data links. The
    /// `queue_size` is used for the viona device that underpins the PCI port
    /// going to the guest. The `uart` is used to provide a P4 management
    /// interface to the guest. The pipeline object is used to process packets.
    /// In most cases the value in the mutex should be initialized to `None` as
    /// users will dynamically load a P4 program from inside the guest.
    pub fn new(
        data_links: Vec<String>,
        queue_size: u16,
        uart: Arc<LpcUart>,
        p9fs: Arc<PciVirtio9pfs>,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        log: Logger,
    ) -> Result<Arc<Self>> {
        info!(log, "softnpu: data links {:#?}", data_links);

        let data_handles = Self::data_handles(&data_links)?;
        let virtio = Arc::new(PortVirtioState::new(queue_size));
        let pci_port = PciVirtioSoftNpuPort::new(
            Self::generate_mac(),
            data_handles,
            virtio,
            pipeline.clone(),
            log.clone(),
        );

        Ok(Arc::new(SoftNpu {
            data_links,
            pci_port,
            uart,
            p9fs,
            booted: Mutex::new(false),
            log,
        }))
    }

    /// Generate a mac address with the Oxide OUI for the leading bits and then
    /// something random in the range of 0xf00000 - 0xf00000 per RFD 174.
    fn generate_mac() -> [u8; 6] {
        let mut rng = rand::thread_rng();
        let m = rng.gen_range::<u32, _>(0xf00000..0xffffff).to_le_bytes();
        [0xa8, 0x40, 0x25, m[0], m[1], m[2]]
    }

    /// Set up a dlpi handle for each data link.
    fn data_handles(data_links: &Vec<String>) -> Result<Vec<dlpi::DlpiHandle>> {
        let mut handles = Vec::new();
        for x in data_links {
            let h = dlpi::open(x, dlpi::sys::DLPI_RAW)?;

            // Although we bind to the IPv6 SAP (Ethertype), the DL_PROMISC_SAP
            // allows us to pick up everything. Binding to *something* to start
            // with appears to be required to get packets.
            dlpi::bind(h, 0x86dd)?;
            dlpi::promisc_on(h, dlpi::sys::DL_PROMISC_MULTI)?;
            dlpi::promisc_on(h, dlpi::sys::DL_PROMISC_SAP)?;
            dlpi::promisc_on(h, dlpi::sys::DL_PROMISC_PHYS)?;
            dlpi::promisc_on(h, dlpi::sys::DL_PROMISC_RX_ONLY)?;
            handles.push(h);
        }
        Ok(handles)
    }

    /// Start the management handler for servicing requests from the guest over
    /// the provided uart device.
    fn run_management_handler_thread(&self) {
        info!(self.log, "softnpu: running management handler");
        self.uart.set_autodiscard(false);

        let log = self.log.clone();
        let uart = self.uart.clone();
        let pipeline = self.pci_port.pipeline.clone();
        let radix = self.data_links.len();

        spawn(move || Self::management_handler(uart, pipeline, radix, log));
    }

    fn management_handler(
        uart: Arc<LpcUart>,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        radix: usize,
        log: Logger,
    ) {
        info!(log, "management handler thread started");
        loop {
            let r = ManagementMessageReader::new(uart.clone(), log.clone());
            let msg = r.read();
            info!(log, "received management message: {:#?}", msg);

            let pipeline = pipeline.clone();
            let uart = uart.clone();
            let log = log.clone();
            handle_management_message(msg, pipeline, uart, radix, log.clone());
            info!(log, "handled management message");
        }
    }
}

impl Lifecycle for SoftNpu {
    fn type_name(&self) -> &'static str {
        "softnpu"
    }

    fn start(&self) -> anyhow::Result<()> {
        let mut booted = self.booted.lock().unwrap();
        if *booted {
            return Ok(());
        }
        self.run_management_handler_thread();
        for i in 0..self.pci_port.data_handles.len() {
            info!(self.log, "starting ingress packet handler for port {}", i);

            PacketHandler::run_ingress_packet_handler_thread(
                i,
                self.pci_port.data_handles.clone(),
                self.pci_port.virtio_state.clone(),
                self.pci_port.pipeline.clone(),
                self.log.clone(),
            );
        }
        *booted = true;
        Ok(())
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::NonMigratable
    }
}

impl PciVirtioSoftNpuPort {
    pub fn new(
        mac: [u8; 6],
        data_handles: Vec<dlpi::DlpiHandle>,
        virtio: Arc<PortVirtioState>,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        log: Logger,
    ) -> Arc<Self> {
        Arc::new(PciVirtioSoftNpuPort {
            mac,
            data_handles,
            pipeline,
            log,
            virtio_state: virtio,
        })
    }

    fn handle_guest_virtio_request(&self, vq: &Arc<VirtQueue>) {
        if vq.id == 0 {
            return self.handle_q0_req(vq);
        }

        let mem = match self.virtio_state.pci_state.acc_mem.access() {
            Some(mem) => mem,
            None => return,
        };
        let mut chain = Chain::with_capacity(1);
        let Some((_idx, _clen)) = vq.pop_avail(&mut chain, &mem) else {
            return;
        };

        // only vq.push_used if we actually read something
        let mut push_used = false;

        // read as many Ethernet frames from the guest as we can
        loop {
            let mut virtio_bytes = [0u8; 10];
            // read in virtio mystery bytes
            let n = read_buf(&mem, &mut chain, &mut virtio_bytes);
            if n != 10 {
                if n > 0 {
                    push_used = true;
                } else {
                    break;
                }
                warn!(self.log, "failed to read virtio mystery bytes ({})", n);
                //break;
            }

            let mut frame = [0u8; MTU];
            // read in Ethernet header
            let n = read_buf(&mem, &mut chain, &mut frame);
            if n == 0 {
                break;
            }
            push_used = true;

            let pkt = packet_in::new(&frame[..n]);

            let mut pipeline = match self.pipeline.lock() {
                Ok(pipe) => pipe,
                Err(e) => {
                    error!(self.log, "failed to lock pipeline: {}", e);
                    break;
                }
            };
            let pl: &mut Box<dyn Pipeline> = match &mut *pipeline {
                Some(ref mut x) => &mut x.1,
                None => {
                    // This just means no P4 program has been set by the guest.
                    break;
                }
            };

            PacketHandler::process_guest_packet(
                pkt,
                &self.data_handles,
                pl,
                &self.log,
            );
        }

        if push_used {
            vq.push_used(&mut chain, &mem);
        }
    }

    fn handle_q0_req(&self, _vq: &Arc<VirtQueue>) {
        // ignore notifications from the queue that we use for writing to the
        // guest.
        return;
    }

    fn net_cfg_read(&self, id: &NetReg, ro: &mut ReadOp) {
        match id {
            NetReg::Mac => {
                ro.write_bytes(&self.mac);
            }
            NetReg::Status => {
                // Always report link up
                ro.write_u16(VIRTIO_NET_S_LINK_UP);
            }
            NetReg::MaxVqPairs => {
                // hard-wired to single vq pair for now
                ro.write_u16(1);
            }
        }
    }
}

impl Lifecycle for PciVirtioSoftNpuPort {
    fn type_name(&self) -> &'static str {
        "pci-virtio-softnpu-port"
    }

    fn reset(&self) {
        self.virtio_state.pci_virtio_state.reset(self);
    }
}

impl PciVirtio for PciVirtioSoftNpuPort {
    fn virtio_state(&self) -> &PciVirtioState {
        &self.virtio_state.pci_virtio_state
    }

    fn pci_state(&self) -> &pci::DeviceState {
        &self.virtio_state.pci_state
    }
}

impl VirtioDevice for PciVirtioSoftNpuPort {
    fn cfg_rw(&self, mut rwo: RWOp) {
        NET_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => self.net_cfg_read(id, ro),
            RWOp::Write(_) => {
                //ignore writes
            }
        });
    }

    fn get_features(&self) -> u32 {
        VIRTIO_NET_F_MAC
    }

    fn set_features(&self, _feat: u32) -> std::result::Result<(), ()> {
        Ok(())
    }

    fn queue_notify(&self, vq: &Arc<VirtQueue>) {
        self.handle_guest_virtio_request(vq);
    }
}

struct PacketHandler {}

impl PacketHandler {
    /// Spawn a thread that handles packets coming into the emulated ASIC for
    /// the specified interface index.
    fn run_ingress_packet_handler_thread(
        index: usize,
        data_handles: Vec<dlpi::DlpiHandle>,
        virtio: Arc<PortVirtioState>,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        log: Logger,
    ) {
        spawn(move || {
            info!(log, "ingress packet handler is running for port {}", index,);
            Self::run_ingress_packet_handler(
                index,
                data_handles,
                virtio.clone(),
                pipeline.clone(),
                log,
            )
        });
    }

    /// Handle packets coming into the emulated ASIC for the specified interface
    /// index.
    fn run_ingress_packet_handler(
        index: usize,
        data_handles: Vec<dlpi::DlpiHandle>,
        virtio: Arc<PortVirtioState>,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        log: Logger,
    ) {
        let dh = data_handles[index];
        loop {
            //
            // wait for a packet from dlpi
            //
            let mut src = [0u8; dlpi::sys::DLPI_PHYSADDR_MAX];
            let mut msg = [0u8; MTU];
            let mut recvinfo = dlpi_recvinfo_t::default();
            let n = match dlpi::recv(
                dh,
                &mut src,
                &mut msg,
                -1, // block until we get something
                Some(&mut recvinfo),
            ) {
                Ok((_, n)) => n,
                Err(e) => {
                    error!(log, "rx error at index {}: {}", index, e);
                    continue;
                }
            };

            //
            // process packet with loaded P4 program
            //

            // TODO pipeline should not need to be mutable for packet handling?
            let pkt = packet_in::new(&msg[..n]);
            let mut p = pipeline.lock().unwrap();
            let pl = match &mut *p {
                Some(ref mut pl) => &mut pl.1,
                None => continue, // no program is loaded
            };

            Self::process_external_packet(
                index + 1,
                pkt,
                &data_handles,
                &virtio,
                pl,
                &log,
            )
        }
    }

    /// Run a packet coming into the ASIC from an external port through the
    /// loaded pipeline and forward it on to its destination.
    fn process_external_packet(
        index: usize,
        mut pkt: packet_in<'_>,
        data_handles: &Vec<dlpi::DlpiHandle>,
        virtio: &Arc<PortVirtioState>,
        pipeline: &mut Box<dyn Pipeline>,
        log: &Logger,
    ) {
        for (mut out_pkt, port) in
            pipeline.process_packet(index as u16, &mut pkt)
        {
            // packet is going to CPU port
            if port == 0 {
                Self::send_packet_to_cpu_port(&mut out_pkt, virtio, &log);
            }
            // packet is passing through
            else {
                Self::send_packet_to_ext_port(
                    &mut out_pkt,
                    data_handles,
                    port - 1,
                    &log,
                );
            }
        }
    }

    /// Run a packet coming into the ASIC from the guest pci port through the
    /// loaded pipeline and forward it on to its destination.
    fn process_guest_packet(
        mut pkt: packet_in<'_>,
        data_handles: &Vec<dlpi::DlpiHandle>,
        pipeline: &mut Box<dyn Pipeline>,
        log: &Logger,
    ) {
        for (mut out_pkt, port) in pipeline.process_packet(0, &mut pkt) {
            if port == 0 {
                // no looping packets back to the guest
                return;
            }
            if port == SOFTNPU_CPU_AUX_PORT {
                // we are not currently emulating this port type
                return;
            }
            Self::send_packet_to_ext_port(
                &mut out_pkt,
                data_handles,
                port - 1,
                &log,
            );
        }
    }

    /// Send a packet out an external port using dlpi.
    fn send_packet_to_ext_port(
        pkt: &mut packet_out<'_>,
        data_handles: &Vec<dlpi::DlpiHandle>,
        port: u16,
        log: &Logger,
    ) {
        if usize::from(port) >= data_handles.len() {
            error!(log, "port out of range {} >= {}", port, data_handles.len());
            return;
        }
        // get the dlpi handle for this port
        let dh = data_handles[port as usize];

        //TODO avoid copying the whole packet
        let mut out = pkt.header_data.clone();
        out.extend_from_slice(pkt.payload_data);

        if let Err(e) = dlpi::send(dh, &[], out.as_slice(), None) {
            error!(log, "tx (ext,0): {}", e);
        }
    }

    /// Send a packet out the guest pci port using virtio.
    fn send_packet_to_cpu_port(
        pkt: &mut packet_out<'_>,
        virtio: &Arc<PortVirtioState>,
        log: &Logger,
    ) {
        let mem = match virtio.pci_state.acc_mem.access() {
            Some(mem) => mem,
            None => {
                warn!(log, "send packet to guest: no guest virtio memory");
                return;
            }
        };
        let mut chain = Chain::with_capacity(1);
        let vq = &virtio.pci_virtio_state.queues[0];
        if let None = vq.pop_avail(&mut chain, &mem) {
            return;
        }

        // write the virtio mystery bytes
        write_buf(&[0u8; 10], &mut chain, &mem);
        write_buf(pkt.header_data.as_mut_slice(), &mut chain, &mem);
        write_buf(pkt.payload_data, &mut chain, &mem);

        vq.push_used(&mut chain, &mem);
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum NetReg {
    Mac,
    Status,
    MaxVqPairs,
}
lazy_static! {
    static ref NET_DEV_REGS: RegMap<NetReg> = {
        let layout =
            [(NetReg::Mac, 6), (NetReg::Status, 2), (NetReg::MaxVqPairs, 2)];
        RegMap::create_packed(VIRTIO_NET_CFG_SIZE, &layout, None)
    };
}

mod bits {
    pub const VIRTIO_NET_CFG_SIZE: usize = 0xa;
}
use bits::*;

// helper functions to read/write a buffer from/to a guest
fn read_buf(mem: &MemCtx, chain: &mut Chain, buf: &mut [u8]) -> usize {
    let mut done = 0;
    chain.for_remaining_type(true, |addr, len| {
        let remain = &mut buf[done..];
        if let Some(copied) = mem.read_into(addr, remain, len) {
            let need_more = copied != remain.len();
            done += copied;
            (copied, need_more)
        } else {
            (0, false)
        }
    })
}

/// Handle ASIC management messages from the guest using the loaded program.
fn handle_management_message(
    msg: ManagementRequest,
    pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
    uart: Arc<LpcUart>,
    radix: usize,
    log: Logger,
) {
    let mut pl_opt = pipeline.lock().unwrap();

    match msg {
        ManagementRequest::TableAdd(tm) => {
            let pl = match &mut *pl_opt {
                Some(pl) => pl,
                None => return,
            };
            pl.1.add_table_entry(
                &tm.table,
                &tm.action,
                &tm.keyset_data,
                &tm.parameter_data,
                0,
            );
        }
        ManagementRequest::TableRemove(tm) => {
            let pl = match &mut *pl_opt {
                Some(pl) => pl,
                None => return,
            };
            pl.1.remove_table_entry(&tm.table, &tm.keyset_data);
        }
        ManagementRequest::RadixRequest => {
            // the data is being sent back as ascii text because this is the
            // simplest way for the guest tty device to handle the data. control
            // characters coming through the pipe are acted on differently and
            // illumos does not currently have a raw mode for termio.
            //
            // - https://code.illumos.org/c/illumos-gate/+/1808
            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(radix.to_string().as_bytes());
            buf.push(b'\n');
            for b in &buf {
                while !uart.write(*b) {
                    std::thread::yield_now();
                }
            }
            info!(log, "wrote: {:?}", buf.len());
        }
        ManagementRequest::DumpRequest => {
            info!(log, "dumping state");
            let result = {
                let pl = match &mut *pl_opt {
                    Some(pl) => &pl.1,
                    None => return,
                };

                // Create a response where a vector of table entries for each
                // table is indexed by the table id.
                let mut result = BTreeMap::new();

                for id in pl.get_table_ids() {
                    let entries = pl.get_table_entries(id);
                    result.insert(id, entries);
                }
                result
            };

            let buf = match serde_json::to_string(&result) {
                Ok(j) => {
                    let mut buf = j.as_bytes().to_vec();
                    info!(log, "writing: {}", j);
                    // Add trailing newline for proper tty handling.
                    buf.push(b'\n');
                    buf
                }
                Err(e) => {
                    warn!(log, "failed to serialize table state: {}", e);
                    b"{}\n".to_vec()
                }
            };

            for b in &buf {
                while !uart.write(*b) {
                    // If we cannot write to the uart, yield and come back once
                    // scheduled again.
                    std::thread::yield_now();
                }
            }

            info!(log, "management wrote: {}", buf.len());
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TableDump {
    pub tables: BTreeMap<String, Vec<p4rs::TableEntry>>,
}

struct ManagementMessageReader {
    uart: Arc<LpcUart>,
    log: Logger,
}

impl ManagementMessageReader {
    fn new(uart: Arc<LpcUart>, log: Logger) -> Self {
        Self { uart, log }
    }

    fn read(&self) -> ManagementRequest {
        loop {
            let mut buf = vec![0; 10240];
            let mut i = 0;
            let mut in_message = false;
            loop {
                let x = match self.uart.read() {
                    Some(b) => b,
                    None => {
                        // If we are in the middle of reading a message come
                        // back in a tight loop. Otherwise check back less
                        // regularly.
                        if in_message {
                            std::thread::yield_now();
                        } else {
                            sleep(Duration::from_millis(100));
                        }
                        continue;
                    }
                };
                if x == b'\n' {
                    break;
                }
                in_message = true;
                buf[i] = x;
                i += 1;
            }
            buf.resize(i, 0);
            // Ttys do cruel and unusual things to our messages.
            buf.retain(|x| *x != b'\r' && *x != b'\0');
            // Find the premable and push the buffer beyond that point.
            let msgbuf = match buf
                .iter()
                .position(|b| *b == MANAGEMENT_MESSAGE_PREAMBLE)
            {
                Some(p) => {
                    if p + 1 < buf.len() {
                        &buf[p + 1..]
                    } else {
                        continue;
                    }
                }
                None => continue,
            };
            match serde_json::from_slice(&msgbuf) {
                Ok(msg) => return msg,
                Err(e) => {
                    error!(self.log, "mgmt message deser: {}", e);
                    error!(self.log, "{:x?}", msgbuf);
                    error!(self.log, "{}", String::from_utf8_lossy(msgbuf));
                    continue;
                }
            }
        }
    }
}

pub struct SoftNpuP9Handler {
    source: String,
    target: String,
    radix: u16,
    log: Logger,
    pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
}

/// The file that a P4 program is written to while being streamed by the guest.
fn p4_temp_file() -> String {
    format!("/tmp/p4_tmp_{}.so", std::process::id())
}

/// The file that is dynamically loaded onto the ASIC.
fn p4_active_file() -> String {
    format!("/tmp/p4_active_{}.p4", std::process::id())
}

impl SoftNpuP9Handler {
    pub fn new(
        source: String,
        target: String,
        radix: u16,
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        log: Logger,
    ) -> Self {
        Self { source, target, radix, pipeline, log }
    }

    /// This function is called while the program is being streamed in from the
    /// guest. The program is incrementally written to a temporary file while
    /// the program is being loaded. A temporary file is used to prevent the
    /// active program's file from being written to while it is being run.
    /// Writing to a file that is mapped by `dlopen` causes explosions.
    fn write_program(buf: &[u8], offset: u64, log: &Logger) {
        info!(log, "loading {} byte program", buf.len());
        let path = p4_temp_file();
        let mut file = match offset {
            // This is the first write, so open the file in create mode.
            0 => match File::create(&path) {
                Ok(f) => f,
                Err(e) => {
                    error!(log, "failed to create p4 file {}: {}", &path, e);
                    return;
                }
            },
            // This is a subsequent write, so open the file win append mode.
            _ => {
                match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        error!(
                            log,
                            "failed to create p4 file {}: {}", &path, e
                        );
                        return;
                    }
                }
            }
        };

        if let Err(e) = file.write_all(&buf) {
            error!(log, "writing p4 program to file failed: {}", e);
            return;
        }
    }

    /// This function is called after a program has been completely copied from
    /// the guest. The current pipeline is dropped. Then the temporary program
    /// file is copied to the active program file. Then the pipeline is loaded
    /// from the active program file.
    fn load_program(
        pipeline: Arc<Mutex<Option<LoadedP4Program>>>,
        radix: u16,
        log: Logger,
    ) {
        let mut pl = pipeline.lock().unwrap();
        // drop anything that may already be loaded before attempting a dlopen
        if let Some((lib, pipe)) = pl.take() {
            // This order is very important, if the lib gets dropped before the
            // pipe the world explodes.
            drop(pipe);
            drop(lib);
        }

        let temp_path = p4_temp_file();
        let active_path = p4_active_file();

        if let Err(e) = fs::copy(&temp_path, &active_path) {
            warn!(log, "copying p4 program file failed: {}", e);
            return;
        }

        let lib = match unsafe { Library::open(Some(&active_path), RTLD_NOW) } {
            Ok(l) => l,
            Err(e) => {
                warn!(log, "failed to load p4 program: {}", e);
                return;
            }
        };
        let func: Symbol<unsafe extern "C" fn(u16) -> *mut dyn p4rs::Pipeline> =
            match unsafe { lib.get(b"_main_pipeline_create") } {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        log,
                        "failed to load _main_pipeline_create func: {}", e
                    );
                    return;
                }
            };

        // account for CPU port
        let radix = radix + 1;
        let boxpipe = unsafe { Box::from_raw(func(radix)) };
        let _ = pl.insert((lib, boxpipe));
    }
}

/// Implement a very specific P9 handler that only implements file writes in
/// order to load P4 programs.
impl P9Handler for SoftNpuP9Handler {
    fn source(&self) -> &str {
        &self.source
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn msize(&self) -> u32 {
        65536
    }

    fn handle_version(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let mut msg: p9ds::proto::Version = match ispf::from_bytes_le(&msg_buf)
        {
            Err(e) => {
                error!(self.log, "could not parse p9fs version message: {}", e);
                return;
            }
            Ok(m) => m,
        };
        msg.typ = p9ds::proto::MessageType::Rversion;

        // This is a version of our own making. It's meant to deter clients that
        // may discover us from trying to use us as some sort of normal P9
        // file system. It also helps clients that are actually looking for the
        // SoftNpu P9 device to identify us as such.
        msg.version = "9P2000.P4".to_owned();

        let mut out = ispf::to_bytes_le(&msg).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn handle_attach(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_walk(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_open(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_readdir(
        &self,
        _msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        _msize: u32,
    ) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_read(
        &self,
        _msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        _msize: u32,
    ) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_write(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        _msize: u32,
    ) {
        let msg: Twrite = ispf::from_bytes_le(&msg_buf).unwrap();
        let len = msg.data.len();

        Self::write_program(&msg.data, msg.offset, &self.log);

        let response = Rwrite::new(len as u32);
        let mut out = ispf::to_bytes_le(&response).unwrap();
        let buf = out.as_mut_slice();
        return write_buf(buf, chain, mem);
    }

    fn handle_clunk(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let pipe = self.pipeline.clone();
        let log = self.log.clone();
        let radix = self.radix;

        spawn(move || Self::load_program(pipe, radix, log));

        let response = Rclunk::new();
        let mut out = ispf::to_bytes_le(&response).unwrap();
        let buf = out.as_mut_slice();
        return write_buf(buf, chain, mem);
    }

    fn handle_getattr(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_statfs(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        write_error(ENOTSUP as u32, chain, &mem)
    }
}
