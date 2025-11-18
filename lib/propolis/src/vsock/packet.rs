use crate::vsock::VSOCK_HOST_CID;

pub(crate) const VIRTIO_VSOCK_OP_REQUEST: VsockPacketOp = 1;
pub(crate) const VIRTIO_VSOCK_OP_RESPONSE: VsockPacketOp = 2;
pub(crate) const VIRTIO_VSOCK_OP_RST: VsockPacketOp = 3;
pub(crate) const VIRTIO_VSOCK_OP_SHUTDOWN: VsockPacketOp = 4;
pub(crate) const VIRTIO_VSOCK_OP_RW: VsockPacketOp = 5;
pub(crate) const VIRTIO_VSOCK_OP_CREDIT_UPDATE: VsockPacketOp = 6;
pub(crate) const VIRTIO_VSOCK_OP_CREDIT_REQUEST: VsockPacketOp = 7;
type VsockPacketOp = u16;

pub(crate) const VIRTIO_VSOCK_TYPE_STREAM: VsockSocketType = 1;
type VsockSocketType = u16;

/// Shutdown flags for VIRTIO_VSOCK_OP_SHUTDOWN
pub(crate) const VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE: u32 = 1;
pub(crate) const VIRTIO_VSOCK_SHUTDOWN_F_SEND: u32 = 2;

#[derive(thiserror::Error, Debug)]
pub enum VsockPacketError {
    #[error("Failed to read packet header from descriptor chain")]
    ChainHeaderRead,
    #[error("Packet only contained {remaining} bytes out of {expected} bytes")]
    InsufficientBytes { expected: usize, remaining: usize },
}

#[repr(C, packed)]
#[derive(Copy, Clone, Default, Debug)]
pub struct VsockPacketHeader {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    // Note this is "type" in the spec
    socket_type: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl VsockPacketHeader {
    pub fn src_cid(&self) -> u64 {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        u64::from_le(self.src_cid) & u64::from(u32::MAX)
    }

    pub fn dst_cid(&self) -> u64 {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        u64::from_le(self.dst_cid) & u64::from(u32::MAX)
    }

    pub fn src_port(&self) -> u32 {
        u32::from_le(self.src_port)
    }

    pub fn dst_port(&self) -> u32 {
        u32::from_le(self.dst_port)
    }

    pub fn len(&self) -> u32 {
        u32::from_le(self.len)
    }

    pub fn socket_type(&self) -> u16 {
        u16::from_le(self.socket_type)
    }

    pub fn op(&self) -> u16 {
        u16::from_le(self.op)
    }

    pub fn flags(&self) -> u32 {
        u32::from_le(self.flags)
    }

    pub fn buf_alloc(&self) -> u32 {
        u32::from_le(self.buf_alloc)
    }

    pub fn fwd_cnt(&self) -> u32 {
        u32::from_le(self.fwd_cnt)
    }

    pub fn set_src_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.src_cid = cid.to_le() as u64;
        self
    }

    pub fn set_dst_cid(&mut self, cid: u32) -> &mut Self {
        // The spec states:
        //
        // The upper 32 bits of src_cid and dst_cid are reserved and zeroed.
        self.dst_cid = cid.to_le() as u64;
        self
    }

    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        self.src_port = port.to_le();
        self
    }

    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        self.dst_port = port.to_le();
        self
    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        self.len = len.to_le();
        self
    }

    pub fn set_socket_type(&mut self, socket_type: u16) -> &mut Self {
        self.socket_type = socket_type.to_le();
        self
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        self.op = op.to_le();
        self
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        self.flags = flags.to_le();
        self
    }

    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        self.buf_alloc = buf_alloc.to_le();
        self
    }

    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        self.fwd_cnt = fwd_cnt.to_le();
        self
    }
}

#[derive(Default, Debug)]
pub struct VsockPacket {
    pub(crate) header: VsockPacketHeader,
    pub(crate) data: Vec<u8>,
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
