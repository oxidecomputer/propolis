// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use bit_field::BitField;
use strum::FromRepr;
use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::vsock::proxy::CONN_TX_BUF_SIZE;
use crate::vsock::VSOCK_HOST_CID;

bitflags! {
    /// Shutdown flags for VIRTIO_VSOCK_OP_SHUTDOWN
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(transparent)]
    pub struct VsockPacketFlags: u32 {
        const VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE = 1;
        const VIRTIO_VSOCK_SHUTDOWN_F_SEND = 2;
    }
}

#[derive(Debug, Clone, Copy, FromRepr, PartialEq, Eq)]
#[repr(u16)]
pub enum VsockSocketType {
    Stream = 1,
    SeqPacket = 2,
    #[cfg(test)]
    InvalidTestValue = 0x1de,
}

#[derive(thiserror::Error, Debug)]
pub enum VsockPacketError {
    #[error("failed to read packet header from descriptor chain")]
    ChainHeaderRead,

    #[error("vsock packet header reported {hdr_len} bytes but the descriptor chain cont ains {chain_len}")]
    InvalidPacketLen { hdr_len: usize, chain_len: usize },

    #[error("descriptor chain only yielded {remaining} bytes out of {expected} bytes")]
    InsufficientBytes { expected: usize, remaining: usize },

    #[error("src_cid {src_cid} contains reserved bits")]
    InvalidSrcCid { src_cid: u64 },

    #[error("dst_cid {dst_cid} contains reserved bits")]
    InvalidDstCid { dst_cid: u64 },
}

#[derive(Clone, Copy, Debug, FromRepr, Eq, PartialEq)]
#[repr(u16)]
pub enum VsockPacketOp {
    Request = 1,
    Response = 2,
    Reset = 3,
    Shutdown = 4,
    ReadWrite = 5,
    CreditUpdate = 6,
    CreditRequest = 7,
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
        self.src_cid.get().get_bits(0..32)
    }

    pub fn dst_cid(&self) -> u64 {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.dst_cid.get().get_bits(0..32)
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

    pub fn socket_type(&self) -> Option<VsockSocketType> {
        VsockSocketType::from_repr(self.socket_type.get())
    }

    pub fn op(&self) -> Option<VsockPacketOp> {
        VsockPacketOp::from_repr(self.op.get())
    }

    pub fn flags(&self) -> VsockPacketFlags {
        VsockPacketFlags::from_bits_retain(self.flags.get())
    }

    pub fn buf_alloc(&self) -> u32 {
        self.buf_alloc.get()
    }

    pub fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt.get()
    }

    pub const fn new() -> Self {
        Self {
            src_cid: U64::new(0),
            dst_cid: U64::new(0),
            src_port: U32::new(0),
            dst_port: U32::new(0),
            len: U32::new(0),
            socket_type: U16::new(VsockSocketType::Stream as u16),
            op: U16::new(0),
            flags: U32::new(0),
            buf_alloc: U32::new(CONN_TX_BUF_SIZE as u32),
            fwd_cnt: U32::new(0),
        }
    }

    pub const fn set_src_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.src_cid = U64::new(cid as u64);
        self
    }

    pub const fn set_dst_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.dst_cid = U64::new(cid as u64);
        self
    }

    pub const fn set_src_port(&mut self, port: u32) -> &mut Self {
        self.src_port = U32::new(port);
        self
    }

    pub const fn set_dst_port(&mut self, port: u32) -> &mut Self {
        self.dst_port = U32::new(port);
        self
    }

    pub const fn set_len(&mut self, len: u32) -> &mut Self {
        self.len = U32::new(len);
        self
    }

    pub const fn set_socket_type(
        &mut self,
        socket_type: VsockSocketType,
    ) -> &mut Self {
        self.socket_type = U16::new(socket_type as u16);
        self
    }

    pub const fn set_op(&mut self, op: VsockPacketOp) -> &mut Self {
        self.op = U16::new(op as u16);
        self
    }

    pub const fn set_flags(&mut self, flags: VsockPacketFlags) -> &mut Self {
        self.flags = U32::new(flags.bits());
        self
    }

    pub const fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        self.buf_alloc = U32::new(buf_alloc);
        self
    }

    pub const fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        self.fwd_cnt = U32::new(fwd_cnt);
        self
    }
}

#[derive(Default)]
pub struct VsockPacket {
    pub(crate) header: VsockPacketHeader,
    pub(crate) data: Box<[u8]>,
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
    fn new(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        op: VsockPacketOp,
    ) -> Self {
        let mut header = VsockPacketHeader::new();
        header
            .set_src_cid(VSOCK_HOST_CID as u32)
            .set_dst_cid(guest_cid)
            .set_src_port(src_port)
            .set_dst_port(dst_port)
            .set_op(op);

        Self { header, data: [].into() }
    }

    pub fn new_reset(guest_cid: u32, src_port: u32, dst_port: u32) -> Self {
        Self::new(guest_cid, src_port, dst_port, VsockPacketOp::Reset)
    }

    pub fn new_response(guest_cid: u32, src_port: u32, dst_port: u32) -> Self {
        let packet =
            Self::new(guest_cid, src_port, dst_port, VsockPacketOp::Response);
        packet
    }

    /// Create a new RW packet that sets the len field to the size of the data.
    ///
    /// Panics if the supplied data value is greater than u32::MAX as anything
    /// larger would not fit within the peers buf_alloc which is defined as u32.
    pub fn new_rw(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        fwd_cnt: u32,
        data: impl Into<Box<[u8]>>,
    ) -> Self {
        let data = data.into();
        let len = data.len();
        assert!(
            len < u32::MAX as usize,
            "vsock packets should not exceed u32::MAX"
        );
        let mut packet =
            Self::new(guest_cid, src_port, dst_port, VsockPacketOp::ReadWrite);
        packet.header.set_len(len as u32);
        packet.header.set_fwd_cnt(fwd_cnt);
        packet.data = data;
        packet
    }

    pub fn new_credit_update(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        fwd_cnt: u32,
    ) -> Self {
        let mut packet = Self::new(
            guest_cid,
            src_port,
            dst_port,
            VsockPacketOp::CreditUpdate,
        );
        packet.header.set_fwd_cnt(fwd_cnt);
        packet
    }

    pub fn new_shutdown(
        guest_cid: u32,
        src_port: u32,
        dst_port: u32,
        flags: VsockPacketFlags,
        fwd_cnt: u32,
    ) -> Self {
        let mut packet =
            Self::new(guest_cid, src_port, dst_port, VsockPacketOp::Shutdown);
        packet.header.set_fwd_cnt(fwd_cnt);
        packet.header.set_flags(flags);
        packet
    }
}
