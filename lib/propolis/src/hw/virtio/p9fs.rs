// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::convert::TryInto;
use std::fs::{self, File};
use std::mem::size_of;
use std::num::NonZeroU16;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::common::*;
use crate::hw::pci;
use crate::migrate::Migrator;
use crate::util::regmap::RegMap;
use crate::vmm::MemCtx;

use super::bits::*;
use super::pci::{PciVirtio, PciVirtioState};
use super::queue::{write_buf, Chain, VirtQueue, VirtQueues};
use super::VirtioDevice;

use ispf::WireSize;
use lazy_static::lazy_static;
use libc::{
    DT_DIR, DT_REG, EILSEQ, EINVAL, ENOENT, ENOLCK, ENOTSUP, EOVERFLOW, ERANGE,
};
use num_enum::TryFromPrimitive;
use p9ds::proto::{
    self, Dirent, MessageType, P9Version, Qid, QidType, Rattach, Rclunk,
    Rgetattr, Rlerror, Rlopen, Rread, Rreaddir, Rstatfs, Rwalk, Tattach,
    Tgetattr, Tlopen, Tread, Treaddir, Tstatfs, Twalk, Version,
    P9_GETATTR_BASIC,
};
use slog::{warn, Logger};

/// This const is to add headroom into serialized P9 data packets. These packets
/// go through a virtio transport. It's been observed with Linux guests that we
/// cannot fill up an entire `msize` packet running through that transport for
/// RREAD message types, as the packet gets truncated by a small number of bytes
/// (13 is most often observed) and the PDU size will no longer match the the
/// RREAD header stated size.
const P9FS_VIRTIO_READ_HEADROOM: usize = 20;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn p9fs_cfg_read() {}
}

/// This is a work-in-progres P9 filesystem device. It's a minimum viable
/// implementation provide a P9 filesystem to guest. It's been tested with
/// illumos and Linux guests. There are many capabilities that are not yet
/// implemented.
///
/// The design centers around a P9Handler trait that allows various different
/// types of P9 devices to be implemented. This file includes a `HostFSHandler`
/// implementation that allows mounting host filesystems in the guest.
/// Currently filesystems can only be mounted as read-only. Another
/// implementation is in the SoftNpu device that supports P4 program transfer
/// via p9fs.
pub struct PciVirtio9pfs {
    virtio_state: PciVirtioState,
    pci_state: pci::DeviceState,
    handler: Arc<dyn P9Handler>,
}

impl PciVirtio9pfs {
    pub fn new(queue_size: u16, handler: Arc<dyn P9Handler>) -> Arc<Self> {
        let queues = VirtQueues::new(
            NonZeroU16::new(queue_size).unwrap(),
            NonZeroU16::new(1).unwrap(),
        );
        let msix_count = Some(2); //guess
        let (virtio_state, pci_state) = PciVirtioState::create(
            queues,
            msix_count,
            VIRTIO_DEV_9P,
            VIRTIO_SUB_DEV_9P_TRANSPORT,
            pci::bits::CLASS_STORAGE,
            VIRTIO_9P_CFG_SIZE,
        );
        Arc::new(Self { virtio_state, pci_state, handler })
    }
}

impl VirtioDevice for PciVirtio9pfs {
    fn cfg_rw(&self, mut rwo: RWOp) {
        P9FS_DEV_REGS.process(&mut rwo, |id, rwo| match rwo {
            RWOp::Read(ro) => {
                probes::p9fs_cfg_read!(|| ());
                match id {
                    P9fsReg::TagLen => {
                        ro.write_u16(self.handler.target().len() as u16);
                    }
                    P9fsReg::Tag => {
                        let mut bs = [0; 256];
                        for (i, x) in self.handler.target().bytes().enumerate()
                        {
                            if i == 256 {
                                break;
                            }
                            bs[i] = x;
                        }
                        ro.write_bytes(&bs);
                        ro.fill(0);
                    }
                }
            }
            RWOp::Write(_) => {}
        })
    }

    fn get_features(&self) -> u32 {
        VIRTIO_9P_F_MOUNT_TAG
    }

    fn set_features(&self, _feat: u32) -> Result<(), ()> {
        Ok(())
    }

    fn queue_notify(&self, vq: &Arc<VirtQueue>) {
        self.handler.handle_req(vq);
    }
}

impl Lifecycle for PciVirtio9pfs {
    fn type_name(&self) -> &'static str {
        "pci-virtio-9pfs"
    }
    fn reset(&self) {
        self.virtio_state.reset(self);
    }
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::NonMigratable
    }
}

impl PciVirtio for PciVirtio9pfs {
    fn virtio_state(&self) -> &PciVirtioState {
        &self.virtio_state
    }
    fn pci_state(&self) -> &pci::DeviceState {
        &self.pci_state
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum P9fsReg {
    TagLen,
    Tag,
}

lazy_static! {
    static ref P9FS_DEV_REGS: RegMap<P9fsReg> = {
        let layout = [(P9fsReg::TagLen, 2), (P9fsReg::Tag, 256)];
        RegMap::create_packed(VIRTIO_9P_CFG_SIZE, &layout, None)
    };
}

struct Fid {
    pathbuf: PathBuf,
    file: Option<fs::File>,
}

struct Fileserver {
    fids: HashMap<u32, Fid>,
}

pub(crate) mod bits {
    use std::mem::size_of;

    // features
    pub const VIRTIO_9P_F_MOUNT_TAG: u32 = 0x1;

    pub const VIRTIO_9P_MAX_TAG_SIZE: usize = 256;
    pub const VIRTIO_9P_CFG_SIZE: usize =
        VIRTIO_9P_MAX_TAG_SIZE + size_of::<u16>();
}
use bits::*;

pub trait P9Handler: Sync + Send + 'static {
    fn source(&self) -> &str;
    fn target(&self) -> &str;
    fn msize(&self) -> u32;
    fn handle_version(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_attach(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_walk(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_open(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_readdir(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    );
    fn handle_read(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    );
    fn handle_write(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    );
    fn handle_clunk(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_getattr(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);
    fn handle_statfs(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx);

    fn handle_req(&self, vq: &Arc<VirtQueue>) {
        let mem = vq.acc_mem.access().unwrap();

        let mut chain = Chain::with_capacity(1);
        let (_idx, _clen) = vq.pop_avail(&mut chain, &mem).unwrap();

        //TODO better as uninitialized?
        let mut data = Vec::new();
        let msize = self.msize();
        data.resize(msize as usize, 0);
        let buf = data.as_mut_slice();

        // TODO copy pasta from tail end of Chain::read function. Seemingly
        // cannot use Chain::read as-is because it expects a statically sized
        // type.
        let mut done = 0;
        let _total = chain.for_remaining_type(true, |addr, len| {
            let remain = &mut buf[done..];
            if let Some(copied) = mem.read_into(addr, remain, len) {
                let need_more = copied != remain.len();
                done += copied;
                (copied, need_more)
            } else {
                (0, false)
            }
        });

        let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let typ = MessageType::try_from_primitive(buf[4]).unwrap();

        match typ {
            MessageType::Tversion => {
                self.handle_version(&data[..len], &mut chain, &mem)
            }

            MessageType::Tattach => {
                self.handle_attach(&data[..len], &mut chain, &mem)
            }

            MessageType::Twalk => {
                self.handle_walk(&data[..len], &mut chain, &mem)
            }

            MessageType::Tlopen => {
                self.handle_open(&data[..len], &mut chain, &mem)
            }

            MessageType::Treaddir => {
                self.handle_readdir(&data[..len], &mut chain, &mem, msize)
            }

            MessageType::Tread => {
                self.handle_read(&data[..len], &mut chain, &mem, msize)
            }

            MessageType::Twrite => {
                self.handle_write(&data[..len], &mut chain, &mem, msize)
            }

            MessageType::Tclunk => {
                self.handle_clunk(&data[..len], &mut chain, &mem)
            }

            MessageType::Tgetattr => {
                self.handle_getattr(&data[..len], &mut chain, &mem)
            }

            MessageType::Tstatfs => {
                self.handle_statfs(&data[..len], &mut chain, &mem)
            }

            //TODO: There are many p9fs operations that are not implemented. If
            //      you hit an ENOTSUP, this is the place to start for adding a
            //      new message type handler.
            _ => {
                write_error(ENOTSUP as u32, &mut chain, &mem);
            }
        };

        vq.push_used(&mut chain, &mem);
    }
}

pub struct HostFSHandler {
    max_chunk_size: u32,
    msize: Mutex<u32>,
    source: String,
    target: String,
    fileserver: Mutex<Box<Fileserver>>,
    log: Logger,
}

impl HostFSHandler {
    pub fn new(
        source: String,
        target: String,
        max_chunk_size: u32,
        log: Logger,
    ) -> Self {
        let fileserver =
            Mutex::new(Box::new(Fileserver { fids: HashMap::new() }));
        Self {
            source,
            target,
            max_chunk_size,
            msize: Mutex::new(max_chunk_size),
            fileserver,
            log,
        }
    }

    fn do_read(
        &self,
        msg: &Tread,
        fid: &mut Fid,
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    ) {
        let file = match fid.file {
            Some(ref f) => f,
            None => {
                // the file is not open
                warn!(self.log, "read: file not open: {:?}", &fid.pathbuf,);
                return write_error(EINVAL as u32, chain, mem);
            }
        };
        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                let ecode = e.raw_os_error().unwrap_or(0);
                warn!(
                    self.log,
                    "read: metadata for {:?}: {:?}", &fid.pathbuf, e,
                );
                return write_error(ecode as u32, chain, mem);
            }
        };

        // bail with empty response if offset is greater than file size
        if metadata.len() < msg.offset {
            warn!(
                self.log,
                "read: offset > file size: {} > {}",
                msg.offset,
                metadata.len(),
            );
            let response = Rread::new(Vec::new());
            let mut out = ispf::to_bytes_le(&response).unwrap();
            let buf = out.as_mut_slice();
            return write_buf(buf, chain, mem);
        }

        let read_count = u32::min(msize, msg.count);

        let space_left = read_count as usize
            - size_of::<u32>()          // Rread.size
            - size_of::<MessageType>()  // Rread.typ
            - size_of::<u16>()          // Rread.tag
            - size_of::<u32>()          // Rread.data.len
            - P9FS_VIRTIO_READ_HEADROOM;

        let buflen =
            std::cmp::min(space_left, (metadata.len() - msg.offset) as usize);

        p9_write_file(&file, chain, mem, buflen, msg.offset as i64);
    }

    fn do_statfs(&self, fid: &mut Fid, chain: &mut Chain, mem: &MemCtx) {
        let sfs = unsafe {
            let mut sfs: libc::statvfs = std::mem::zeroed::<libc::statvfs>();
            libc::statvfs(
                fid.pathbuf.as_path().as_os_str().as_bytes().as_ptr()
                    as *const i8,
                &mut sfs,
            );
            sfs
        };

        // fstype: u32
        let fstype = 0;
        // bsize: u32
        let bsize = sfs.f_bsize;
        // blocks: u64
        let blocks = sfs.f_blocks;
        // bfree: u64
        let bfree = sfs.f_bfree;
        // bavail: u64
        let bavail = sfs.f_bavail;
        // files: u64
        let files = sfs.f_files;
        // ffree: u64
        let ffree = sfs.f_ffree;
        // fsid: u64
        let fsid = sfs.f_fsid;
        // namelen: u32
        let namelen = sfs.f_namemax;

        let resp = Rstatfs::new(
            fstype,
            bsize as u32,
            blocks,
            bfree,
            bavail,
            files,
            ffree,
            fsid,
            namelen as u32,
        );

        let mut out = ispf::to_bytes_le(&resp).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn do_getattr(&self, fid: &mut Fid, chain: &mut Chain, mem: &MemCtx) {
        let metadata = match fs::metadata(&fid.pathbuf) {
            Ok(m) => m,
            Err(e) => {
                let ecode = e.raw_os_error().unwrap_or(0);
                return write_error(ecode as u32, chain, mem);
            }
        };

        // valid: u64,
        let valid = P9_GETATTR_BASIC;
        // qid: Qid,
        let qid = Qid {
            typ: {
                if metadata.is_dir() {
                    QidType::Dir
                } else if metadata.is_symlink() {
                    QidType::Link
                } else {
                    QidType::File
                }
            },
            version: metadata.mtime() as u32, //todo something better from ufs?
            path: metadata.ino(),
        };
        // mode: u32,
        let mode = metadata.mode();
        // uid: u32,
        //let uid = metadata.uid();
        let uid = 0; //squash for now
                     // gid: u32,
                     //let gid = metadata.gid();
        let gid = 0; //squash for now
                     // nlink: u64,
        let nlink = metadata.nlink();
        // rdev: u64,
        let rdev = metadata.rdev();
        // attrsize: u64,
        let attrsize = metadata.size();
        // blksize: u64,
        let blksize = metadata.blksize();
        // blocks: u64,
        let blocks = metadata.blocks();
        // atime_sec: u64,
        let atime_sec = metadata.atime();
        // atime_nsec: u64,
        let atime_nsec = metadata.atime_nsec();
        // mtime_sec: u64,
        let mtime_sec = metadata.mtime();
        // mtime_nsec: u64,
        let mtime_nsec = metadata.mtime_nsec();
        // ctime_sec: u64,
        let ctime_sec = metadata.ctime();
        // ctime_nsec: u64,
        let ctime_nsec = metadata.ctime_nsec();
        // btime_sec: u64,
        let btime_sec = 0; // reserved for future use in spec
                           // btime_nsec: u64,
        let btime_nsec = 0; // reserved for future use in spec
                            // gen: u64,
        let gen = 0; // reserved for future use in spec
                     // data_version: u64,
        let data_version = 0; // reserved for future use in spec

        let resp = Rgetattr::new(
            valid,
            qid,
            mode,
            uid,
            gid,
            nlink,
            rdev,
            attrsize,
            blksize,
            blocks,
            atime_sec as u64,
            atime_nsec as u64,
            mtime_sec as u64,
            mtime_nsec as u64,
            ctime_sec as u64,
            ctime_nsec as u64,
            btime_sec as u64,
            btime_nsec as u64,
            gen,
            data_version,
        );

        let mut out = ispf::to_bytes_le(&resp).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }
}

impl P9Handler for HostFSHandler {
    fn source(&self) -> &str {
        &self.source
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn msize(&self) -> u32 {
        match self.msize.lock() {
            Ok(msize) => *msize,
            Err(e) => {
                warn!(self.log, "handle_req: failed to get msize lock: {}", e);
                self.max_chunk_size
            }
        }
    }

    fn handle_version(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let mut msg: Version = ispf::from_bytes_le(&msg_buf).unwrap();
        msg.version = P9Version::V2000L.to_string();
        msg.typ = MessageType::Rversion;
        if msg.msize > self.max_chunk_size {
            warn!(
                self.log,
                "request exceeds max chunk size {} > {}",
                msg.msize,
                self.max_chunk_size
            );
            return write_error(EOVERFLOW as u32, chain, mem);
        }
        // TODO this is likely bad for multiple clients with different msizes,
        // should be a session level variable.
        match self.msize.lock() {
            Ok(mut msize) => *msize = msg.msize,
            Err(e) => {
                warn!(
                    self.log,
                    "handle_version: failed to get msize lock: {}", e
                );
            }
        }
        let mut out = ispf::to_bytes_le(&msg).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn handle_attach(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        //NOTE:
        //  - multiple file trees not supported, aname is ignored
        //  - authentication not supported afid is ignored
        //  - users not tracked, uname is ignored

        // deserialize message
        let msg: Tattach = ispf::from_bytes_le(&msg_buf).unwrap();

        // grab inode number for qid uniqe file id
        let qpath = match fs::metadata(&self.source) {
            Err(e) => {
                let ecode = e.raw_os_error().unwrap_or(0);
                return write_error(ecode as u32, chain, mem);
            }
            Ok(m) => m.ino(),
        };

        match self.fileserver.lock() {
            Ok(mut fs) => {
                // check to see if fid is in use
                match fs.fids.get(&msg.fid) {
                    Some(_) => {
                        warn!(self.log, "attach fid in use: {}", msg.fid);
                        // The spec says to throw an error here, but in an
                        // effort to support clients who don't explicitly cluck
                        // fids, and considering the fact that we do not support
                        // multiple fs trees, just carry on
                        //return write_error(EEXIST as u32, chain, mem);
                    }
                    None => {
                        // create fid entry
                        fs.fids.insert(
                            msg.fid,
                            Fid {
                                pathbuf: PathBuf::from(self.source.clone()),
                                file: None,
                            },
                        );
                    }
                };
            }
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        }

        // send response
        let response =
            Rattach::new(Qid { typ: QidType::Dir, version: 0, path: qpath });
        let mut out = ispf::to_bytes_le(&response).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn handle_walk(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let msg: Twalk = ispf::from_bytes_le(&msg_buf).unwrap();

        match self.fileserver.lock() {
            Ok(mut fs) => {
                // check to see if fid exists
                let fid = match fs.fids.get(&msg.fid) {
                    Some(p) => p,
                    None => {
                        warn!(self.log, "walk: fid {} not found", msg.fid);
                        return write_error(ENOENT as u32, chain, mem);
                    }
                };

                let mut qids = Vec::new();
                let mut newpath = fid.pathbuf.clone();
                if msg.wname.len() > 0 {
                    // create new sub path from referenced fid path and wname
                    // elements
                    for n in msg.wname {
                        newpath.push(n.value);
                    }

                    // check that new path is a thing
                    let (ino, qt) = match fs::metadata(&newpath) {
                        Err(e) => {
                            let ecode = e.raw_os_error().unwrap_or(0);
                            warn!(
                                self.log,
                                "walk: no metadata: {:?}: {:?}", newpath, e
                            );
                            return write_error(ecode as u32, chain, mem);
                        }
                        Ok(m) => {
                            let qt = if m.is_dir() {
                                QidType::Dir
                            } else {
                                QidType::File
                            };
                            (m.ino(), qt)
                        }
                    };
                    qids.push(Qid { typ: qt, version: 0, path: ino });
                }

                // check to see if newfid is in use
                match fs.fids.get(&msg.newfid) {
                    Some(_) => {
                        // The spec says to throw an error here, but in an
                        // effort to support clients who don't explicitly cluck
                        // fids, and considering the fact that we do not support
                        // multiple fs trees, just carry on
                        //return write_error(EEXIST as u32, chain, mem);
                    }
                    None => {}
                };

                // create newfid entry
                fs.fids
                    .insert(msg.newfid, Fid { pathbuf: newpath, file: None });

                let response = Rwalk::new(qids);
                let mut out = ispf::to_bytes_le(&response).unwrap();
                let buf = out.as_mut_slice();
                write_buf(buf, chain, mem);
            }
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        }
    }

    fn handle_open(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let msg: Tlopen = ispf::from_bytes_le(&msg_buf).unwrap();

        match self.fileserver.lock() {
            Ok(mut fs) => {
                // check to see if fid exists
                let fid = match fs.fids.get_mut(&msg.fid) {
                    Some(p) => p,
                    None => {
                        warn!(self.log, "open: fid {} not found", msg.fid);
                        return write_error(ENOENT as u32, chain, mem);
                    }
                };

                // check that fid path is a thing
                let (ino, qt) = match fs::metadata(&fid.pathbuf) {
                    Err(e) => {
                        let ecode = e.raw_os_error().unwrap_or(0);
                        warn!(
                            self.log,
                            "open: no metadata: {:?}: {:?}", &fid.pathbuf, e
                        );
                        return write_error(ecode as u32, chain, mem);
                    }
                    Ok(m) => {
                        let qt = if m.is_dir() {
                            QidType::Dir
                        } else {
                            QidType::File
                        };
                        (m.ino(), qt)
                    }
                };

                // open the file
                fid.file = Some(
                    match fs::OpenOptions::new()
                        .read(true)
                        .open(fid.pathbuf.clone())
                    {
                        Ok(f) => f,
                        Err(e) => {
                            let ecode = e.raw_os_error().unwrap_or(0);
                            warn!(
                                self.log,
                                "open: {:?}: {:?}", &fid.pathbuf, e
                            );
                            return write_error(ecode as u32, chain, mem);
                        }
                    },
                );

                let response =
                    Rlopen::new(Qid { typ: qt, version: 0, path: ino }, 0);

                let mut out = ispf::to_bytes_le(&response).unwrap();
                let buf = out.as_mut_slice();
                write_buf(buf, chain, mem);
            }
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        }
    }

    fn handle_readdir(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    ) {
        let msg: Treaddir = ispf::from_bytes_le(&msg_buf).unwrap();

        // get the path for the requested fid
        let pathbuf = match self.fileserver.lock() {
            Ok(fs) => match fs.fids.get(&msg.fid) {
                Some(f) => f.pathbuf.clone(),
                None => {
                    warn!(self.log, "readdir: fid {} not found", msg.fid);
                    return write_error(ENOENT as u32, chain, mem);
                }
            },
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        };

        // read the directory at the provided path
        let mut dir = match fs::read_dir(&pathbuf) {
            Ok(r) => match r.collect::<Result<Vec<fs::DirEntry>, _>>() {
                Ok(d) => d,
                Err(e) => {
                    let ecode = e.raw_os_error().unwrap_or(0);
                    warn!(
                        self.log,
                        "readdir: collect: {:?}: {:?}", &pathbuf, e
                    );
                    return write_error(ecode as u32, chain, mem);
                }
            },
            Err(e) => {
                let ecode = e.raw_os_error().unwrap_or(0);
                warn!(self.log, "readdir: {:?}: {:?}", &pathbuf, e);
                return write_error(ecode as u32, chain, mem);
            }
        };

        // bail with out of range error if offset is greater than entries
        if (dir.len() as u64) < msg.offset {
            return write_error(ERANGE as u32, chain, mem);
        }

        // need to sort to ensure consistent offsets
        dir.sort_by_key(|a| a.path());

        let mut space_left = msize as usize
            - size_of::<u32>()          // Rreaddir.size
            - size_of::<MessageType>()  // Rreaddir.typ
            - size_of::<u16>()          // Rreaddir.tag
            - size_of::<u32>(); // Rreaddir.data.len

        let mut entries: Vec<proto::Dirent> = Vec::new();

        let mut offset = 1;
        for de in &dir[msg.offset as usize..] {
            let metadata = match de.metadata() {
                Ok(m) => m,
                Err(e) => {
                    let ecode = e.raw_os_error().unwrap_or(0);
                    warn!(
                        self.log,
                        "readdir: metadata: {:?}: {:?}",
                        &de.path(),
                        e
                    );
                    return write_error(ecode as u32, chain, mem);
                }
            };

            let (typ, ftyp) = if metadata.is_dir() {
                (QidType::Dir, DT_DIR)
            } else {
                (QidType::File, DT_REG)
            };

            let qid = Qid { typ, version: 0, path: metadata.ino() };

            let name = match de.file_name().into_string() {
                Ok(n) => n,
                Err(_) => {
                    // getting a bit esoteric with our error codes here...
                    return write_error(EILSEQ as u32, chain, mem);
                }
            };

            let dirent = Dirent { qid, offset, typ: ftyp, name };

            if space_left <= dirent.wire_size() {
                break;
            }

            space_left -= dirent.wire_size();
            entries.push(dirent);
            offset += 1;
        }

        let response = Rreaddir::new(entries);
        let mut out = ispf::to_bytes_le(&response).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn handle_read(
        &self,
        msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        msize: u32,
    ) {
        let msg: Tread = ispf::from_bytes_le(&msg_buf).unwrap();

        // get  the requested fid
        match self.fileserver.lock() {
            Ok(ref mut fs) => match fs.fids.get_mut(&msg.fid) {
                Some(ref mut fid) => self.do_read(&msg, fid, chain, mem, msize),
                None => {
                    warn!(self.log, "read: fid {} not found", msg.fid);
                    return write_error(ENOENT as u32, chain, mem);
                }
            },
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        };
    }

    fn handle_write(
        &self,
        _msg_buf: &[u8],
        chain: &mut Chain,
        mem: &MemCtx,
        _msize: u32,
    ) {
        write_error(ENOTSUP as u32, chain, &mem)
    }

    fn handle_clunk(&self, _msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        //TODO something
        let resp = Rclunk::new();
        let mut out = ispf::to_bytes_le(&resp).unwrap();
        let buf = out.as_mut_slice();
        write_buf(buf, chain, mem);
    }

    fn handle_getattr(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let msg: Tgetattr = ispf::from_bytes_le(&msg_buf).unwrap();
        match self.fileserver.lock() {
            Ok(ref mut fs) => match fs.fids.get_mut(&msg.fid) {
                Some(ref mut fid) => self.do_getattr(fid, chain, mem),
                None => {
                    warn!(self.log, "getattr: fid {} not found", msg.fid);
                    return write_error(ENOENT as u32, chain, mem);
                }
            },
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        }
    }

    fn handle_statfs(&self, msg_buf: &[u8], chain: &mut Chain, mem: &MemCtx) {
        let msg: Tstatfs = ispf::from_bytes_le(&msg_buf).unwrap();
        match self.fileserver.lock() {
            Ok(ref mut fs) => match fs.fids.get_mut(&msg.fid) {
                Some(ref mut fid) => self.do_statfs(fid, chain, mem),
                None => {
                    warn!(self.log, "statfs: fid {} not found", msg.fid);
                    return write_error(ENOENT as u32, chain, mem);
                }
            },
            Err(_) => {
                return write_error(ENOLCK as u32, chain, mem);
            }
        }
    }
}

pub(crate) fn write_error(ecode: u32, chain: &mut Chain, mem: &MemCtx) {
    let msg = Rlerror::new(ecode);
    let mut out = ispf::to_bytes_le(&msg).unwrap();
    let buf = out.as_mut_slice();
    write_buf(buf, chain, mem);
}

fn p9_write_file(
    file: &File,
    chain: &mut Chain,
    mem: &MemCtx,
    count: usize,
    offset: i64,
) {
    // Form the rread header. Unfortunately we can't do this with the Rread
    // structure because the count is baked into the data field which is tied
    // to the length of the vector and filling that vector is what we're
    // explicitly trying to avoid here.
    #[repr(C, packed)]
    #[derive(Copy, Clone)]
    struct Header {
        size: u32,
        typ: u8,
        tag: u16,
        count: u32,
    }

    let size = size_of::<Header>() + count;

    let h = Header {
        size: size as u32,
        typ: MessageType::Rread as u8,
        tag: 0,
        count: count as u32,
    };

    chain.write(&h, mem);

    // Send the header to the guest from the buffer constructed above. Then
    // send the actual file data
    let mut done = 0;
    let _total = chain.for_remaining_type(false, |addr, len| {
        let sub_mapping = mem.writable_region(&GuestRegion(addr, len)).unwrap();
        let len = usize::min(len, count);
        let off = offset + done as i64;
        let mapped = sub_mapping.pread(file, len, off).unwrap();
        done += mapped;

        let need_more = done < count;
        (mapped, need_more)
    });
}
