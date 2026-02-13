// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::vsock::VSOCK_HOST_CID;

pub const VIRTIO_VSOCK_OP_REQUEST: VsockPacketOp = 1;
pub const VIRTIO_VSOCK_OP_RESPONSE: VsockPacketOp = 2;
pub const VIRTIO_VSOCK_OP_RST: VsockPacketOp = 3;
pub const VIRTIO_VSOCK_OP_SHUTDOWN: VsockPacketOp = 4;
pub const VIRTIO_VSOCK_OP_RW: VsockPacketOp = 5;
pub const VIRTIO_VSOCK_OP_CREDIT_UPDATE: VsockPacketOp = 6;
pub const VIRTIO_VSOCK_OP_CREDIT_REQUEST: VsockPacketOp = 7;
type VsockPacketOp = u16;

pub(crate) const VIRTIO_VSOCK_TYPE_STREAM: VsockSocketType = 1;
type VsockSocketType = u16;

/// Shutdown flags for VIRTIO_VSOCK_OP_SHUTDOWN
pub const VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE: u32 = 1;
pub const VIRTIO_VSOCK_SHUTDOWN_F_SEND: u32 = 2;

#[derive(thiserror::Error, Debug)]
pub enum VsockPacketError {
    #[error("failed to read packet header from descriptor chain")]
    ChainHeaderRead,
    #[error("packet only contained {remaining} bytes out of {expected} bytes")]
    InsufficientBytes { expected: usize, remaining: usize },
}

#[repr(C, packed)]
#[derive(Copy, Clone, Default, Debug, FromBytes, IntoBytes, Immutable)]
pub struct VsockPacketHeader {
    src_cid: U64,
    dst_cid: U64,
    src_port: U32,
    dst_port: U32,
    len: U32,
    // Note this is "type" in the spec
    socket_type: U16,
    op: U16,
    flags: U32,
    buf_alloc: U32,
    fwd_cnt: U32,
}

impl VsockPacketHeader {
    pub fn src_cid(&self) -> u64 {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.src_cid.get() & u64::from(u32::MAX)
    }

    pub fn dst_cid(&self) -> u64 {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.dst_cid.get() & u64::from(u32::MAX)
    }

    pub fn src_port(&self) -> u32 {
        self.src_port.get()
    }

    pub fn dst_port(&self) -> u32 {
        self.dst_port.get()
    }

    pub fn len(&self) -> u32 {
        self.len.get()
    }

    pub fn socket_type(&self) -> u16 {
        self.socket_type.get()
    }

    pub fn op(&self) -> u16 {
        self.op.get()
    }

    pub fn flags(&self) -> u32 {
        self.flags.get()
    }

    pub fn buf_alloc(&self) -> u32 {
        self.buf_alloc.get()
    }

    pub fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt.get()
    }

    pub fn set_src_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.src_cid = U64::new(cid as u64);
        self
    }

    pub fn set_dst_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.dst_cid = U64::new(cid as u64);
        self
    }

    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        self.src_port = U32::new(port);
        self
    }

    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        self.dst_port = U32::new(port);
        self
    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        self.len = U32::new(len);
        self
    }

    pub fn set_socket_type(&mut self, socket_type: u16) -> &mut Self {
        self.socket_type = U16::new(socket_type);
        self
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        self.op = U16::new(op);
        self
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        self.flags = U32::new(flags);
        self
    }

    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        self.buf_alloc = U32::new(buf_alloc);
        self
    }

    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        self.fwd_cnt = U32::new(fwd_cnt);
        self
    }
}

#[derive(Default)]
pub struct VsockPacket {
    pub(crate) header: VsockPacketHeader,
    pub(crate) data: Vec<u8>,
}

impl std::fmt::Debug for VsockPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VsockPacket")
            .field("header", &self.header)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl VsockPacket {
    pub fn new_reset(guest_cid: u32, src_port: u32, dst_port: u32) -> Self {
        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RST)
            .set_buf_alloc(0)
            .set_fwd_cnt(0);

        VsockPacket { header, data: Vec::new() }
    }

    pub fn new_response(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        buf_alloc: u32,
    ) -> Self {
        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RESPONSE)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(0);

        VsockPacket { header, data: Vec::new() }
    }

    pub fn new_rw(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        buf_alloc: u32,
        fwd_cnt: u32,
        data: Vec<u8>,
    ) -> Self {
        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(data.len() as u32)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_RW)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(fwd_cnt);

        VsockPacket { header, data }
    }

    pub fn new_credit_update(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        buf_alloc: u32,
        fwd_cnt: u32,
    ) -> Self {
        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_CREDIT_UPDATE)
            .set_buf_alloc(buf_alloc)
            .set_fwd_cnt(fwd_cnt);

        VsockPacket { header, data: Vec::new() }
    }

    pub fn new_shutdown(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        flags: u32,
    ) -> Self {
        let mut header = VsockPacketHeader::default();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_len(0)
            .set_socket_type(VIRTIO_VSOCK_TYPE_STREAM)
            .set_op(VIRTIO_VSOCK_OP_SHUTDOWN)
            .set_flags(flags);

        VsockPacket { header, data: Vec::new() }
    }
}
